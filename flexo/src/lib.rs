#[macro_use] extern crate log;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::thread::JoinHandle;
use std::collections::hash_map::Entry;
use crossbeam::crossbeam_channel::{unbounded, Sender, Receiver};

const NUM_MAX_ATTEMPTS: i32 = 100;

#[derive(Debug)]
pub struct JobPartiallyCompleted<J> where J: Job {
    pub channel: J::C,
    pub continue_at: u64
}

#[derive(Debug)]
pub struct JobTerminated<J> where J: Job {
    pub channel: J::C,
    pub error: J::E,
}

impl <J> JobPartiallyCompleted<J> where J: Job {
    pub fn new(channel: J::C, continue_at: u64) -> Self {
        Self {
            channel,
            continue_at
        }
    }
}

#[derive(Debug)]
pub struct JobCompleted<J> where J: Job {
    pub channel: J::C,
    pub provider: J::P,
    pub size: i64,
}

impl <J> JobCompleted<J> where J: Job {
    pub fn new(channel: J::C, provider: J::P, size: i64) -> Self {
        Self {
            channel,
            provider,
            size,
        }
    }
}

#[derive(Debug)]
pub enum JobResult<J> where J: Job {
    Complete(JobCompleted<J>),
    Partial(JobPartiallyCompleted<J>),
    Error(JobTerminated<J>),
    /// No provider was able to fulfil the order since the order was unavailable at all providers.
    Unavailable(J::C),
    /// The client has specified an invalid order that cannot be served.
    ClientError,
    /// An unexpected internal error has occurred while attempting to process the client's order.
    UnexpectedInternalError,
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum JobOutcome <J> where J: Job {
    Success(J::P),
    Error(HashMap<J::P, i32>),
}

impl <J> JobResult<J> where J: Job {
    fn is_success(&self) -> bool {
        match self {
            JobResult::Complete(_) => true,
            _ => false,
        }
    }
}

pub trait Provider where
    Self: std::marker::Sized + std::fmt::Debug + std::clone::Clone + std::cmp::Eq + std::hash::Hash + std::marker::Send + 'static,
{
    type J: Job;
    fn new_job(&self, properties: &<<Self as Provider>::J as Job>::PR, order: <<Self as Provider>::J as Job>::O) -> Self::J;

    fn initial_score(&self) -> <<Self as Provider>::J as Job>::S;

    /// A short description which will be used in log messages.
    fn description(&self) -> String;

    fn punish(self, mut failures: MutexGuard<HashMap<Self, i32>>) {
        let value = failures.entry(self).or_insert(0);
        *value += 1;
    }

    fn reward(self, mut failures: MutexGuard<HashMap<Self, i32>>) {
        let value = failures.entry(self).or_insert(0);
        *value -= 1;
    }
}

pub trait Job where Self: std::marker::Sized + std::fmt::Debug + std::marker::Send + 'static {
    type S: std::cmp::Ord + core::marker::Copy;
    type JS;
    type C: Channel<J=Self>;
    type O: Order<J=Self> + std::clone::Clone + std::cmp::Eq + std::hash::Hash + std::fmt::Debug;
    type P: Provider<J=Self>;
    type E: std::fmt::Debug;
    type PI: std::cmp::Eq;
    type PR: Properties + std::marker::Send + std::marker::Sync + std::clone::Clone;
    type OE: std::fmt::Debug;

    fn provider(&self) -> &Self::P;
    fn order(&self) -> Self::O;
    fn properties(&self)-> Self::PR;
    fn initialize_cache(properties: Self::PR) -> HashMap<Self::O, OrderState>;
    fn serve_from_provider(self, channel: Self::C, properties: Self::PR, cached_size: u64) -> JobResult<Self>;
    fn handle_error(self, error: Self::OE) -> JobResult<Self>;
    fn acquire_resources(order: &Self::O, properties: &Self::PR, last_chance: bool) -> std::io::Result<Self::JS>;

    fn get_channel(&self, channels: &Arc<Mutex<HashMap<Self::P, Self::C>>>, tx: Sender<FlexoProgress>, last_chance: bool) -> Result<(Self::C, ChannelEstablishment), Self::OE> {
        let mut channels = channels.lock().unwrap();
        match channels.remove(&self.provider()) {
            Some(channel) => {
                info!("Attempt to reuse previous connection from {}", &self.provider().description());
                let result = self.order().reuse_channel(self.properties(), tx, last_chance, channel);
                result.map(|new_channel| {
                    (new_channel, ChannelEstablishment::ExistingChannel)
                })
            }
            None => {
                info!("Establish a new connection to {:?}", &self.provider().description());
                let channel = self.order().new_channel(self.properties(), tx, last_chance);
                channel.map(|c| {
                    (c, ChannelEstablishment::NewChannel)
                })
            }
        }
    }
}

pub struct ProvidersWithStats<J> where J: Job {
    pub provider_failures: Arc<Mutex<HashMap<J::P, i32>>>,
    pub provider_current_usages: Arc<Mutex<HashMap<J::P, i32>>>,
    pub providers: Vec<J::P>,
}


pub trait Order where Self: std::marker::Sized + std::clone::Clone + std::cmp::Eq + std::hash::Hash + std::fmt::Debug + std::marker::Send + 'static {
    type J: Job<O=Self>;
    fn new_channel(self,
                   properties: <<Self as Order>::J as Job>::PR,
                   tx: Sender<FlexoProgress>,
                   last_chance: bool
    ) -> Result<<<Self as Order>::J as Job>::C, <<Self as Order>::J as Job>::OE>;

    fn reuse_channel(self,
                   properties: <<Self as Order>::J as Job>::PR,
                   tx: Sender<FlexoProgress>,
                   last_chance: bool,
                   channel: <<Self as Order>::J as Job>::C,
    ) -> Result<<<Self as Order>::J as Job>::C, <<Self as Order>::J as Job>::OE>;

    fn is_cacheable(&self) -> bool;

    /// If this order can only be served by a custom provider, the identifier of the required provider is returned.
    fn custom_provider(&self) -> Option<<<Self as Order>::J as Job>::P>;

    fn try_until_success(
        self,
        provider_stats: &mut ProvidersWithStats<<Self as Order>::J>,
        channels: Arc<Mutex<HashMap<<<Self as Order>::J as Job>::P, <<Self as Order>::J as Job>::C>>>,
        tx: Sender<FlexoMessage<<<Self as Order>::J as Job>::P>>,
        tx_progress: Sender<FlexoProgress>,
        properties: <<Self as Order>::J as Job>::PR,
        cached_size: u64,
    ) -> JobResult<Self::J> {
        let mut num_attempt = 0;
        let mut punished_providers = Vec::new();
        let result = loop {
            num_attempt += 1;
            debug!("Attempt number {}", num_attempt);
            let (provider, is_last_provider) = match self.custom_provider() {
                None => self.select_provider(provider_stats),
                Some(p) => (p, true),
            };
            debug!("selected provider: {:?}", &provider);
            debug!("No providers are left after this provider? {}", is_last_provider);
            let last_chance = num_attempt >= NUM_MAX_ATTEMPTS || is_last_provider;
            let message = FlexoMessage::ProviderSelected(provider.clone());
            let _ = tx.send(message);
            {
                debug!("Obtain lock on provider_current_usages…");
                let mut provider_current_usages = provider_stats.provider_current_usages.lock().unwrap();
                debug!("Got lock on provider_current_usages.");
                let value = provider_current_usages.entry(provider.clone()).or_insert(0);
                *value += 1;
            }
            let self_cloned: Self = self.clone();
            let job = provider.new_job(&properties, self_cloned);
            debug!("Attempt to establish new connection");
            let channel_result = job.get_channel(&channels, tx_progress.clone(), last_chance);
            let result = match channel_result {
                Ok((channel, channel_establishment)) => {
                    let _ = tx.send(FlexoMessage::ChannelEstablished(channel_establishment));
                    job.serve_from_provider(channel, properties.clone(), cached_size)
                }
                Err(e) => {
                    warn!("Error while attempting to establish a new connection: {:?}", e);
                    let _ = tx.send(FlexoMessage::OrderError);
                    let _ = tx_progress.send(FlexoProgress::OrderError);
                    job.handle_error(e)
                }
            };
            match &result {
                JobResult::Complete(_) => {
                    debug!("Job completed: Rewarding provider {}", provider.description());
                    provider.clone().reward(provider_stats.provider_failures.lock().unwrap());
                },
                JobResult::Partial(partial_job) => {
                    provider.clone().punish(provider_stats.provider_failures.lock().unwrap());
                    punished_providers.push(provider.clone());
                    debug!("Job only partially finished until size {:?}", partial_job.continue_at);
                },
                JobResult::Error(e) => {
                    provider.clone().punish(provider_stats.provider_failures.lock().unwrap());
                    punished_providers.push(provider.clone());
                    info!("Error: {:?}, try again", e)
                },
                JobResult::Unavailable(_) => {
                    info!("Order is not available, let's try again with a different provider.")
                },
                JobResult::ClientError => {
                    warn!("Unable to finish job: {:?}", &result);
                    break result;
                },
                JobResult::UnexpectedInternalError => {
                    warn!("Unable to finish job: {:?}", &result);
                    break result;
                },
            };
            if result.is_success() || provider_stats.providers.is_empty() || last_chance {
                break result;
            }
        };
        if !result.is_success() {
            Self::pardon(punished_providers, provider_stats.provider_failures.lock().unwrap());
        }

        result
    }

    fn select_provider(
        &self,
        provider_stats: &mut ProvidersWithStats<<Self as Order>::J>,
    ) -> (<<Self as Order>::J as Job>::P, bool) {
        let provider_failures = provider_stats.provider_failures.lock().unwrap();
        let provider_current_usages = provider_stats.provider_current_usages.lock().unwrap();
        let (idx, _) = provider_stats.providers
            .iter()
            .map(|x| DynamicScore {
                num_failures: *(provider_failures.get(&x).unwrap_or(&0)),
                num_current_usages: *(provider_current_usages.get(&x).unwrap_or(&0)),
                initial_score: x.initial_score()
            })
            .enumerate()
            .min_by_key(|(_idx, dynamic_score)| *dynamic_score)
            .unwrap();

        let provider = provider_stats.providers.remove(idx);
        (provider, provider_stats.providers.is_empty())
    }

    fn pardon(punished_providers: Vec<<<Self as Order>::J as Job>::P>,
              mut failures: MutexGuard<HashMap<<<Self as Order>::J as Job>::P, i32>>)
    {
        for not_guilty in punished_providers {
            match (*failures).entry(not_guilty.clone()) {
                Entry::Occupied(mut value) => {
                    let value = value.get_mut();
                    *value -= 1;
                },
                Entry::Vacant(_) => {},
            }
        }
    }
}

/// A score that incorporates information that we have gained while using this provider.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
struct DynamicScore <S> where S: Ord {
    num_failures: i32,
    num_current_usages: i32,
    initial_score: S,
}

pub trait Channel where Self: std::marker::Sized + std::fmt::Debug + std::marker::Send + 'static {
    type J: Job;

    fn progress_indicator(&self) -> Option<u64>;
    fn job_state(&mut self) -> &mut JobState<Self::J>;
}

#[derive(PartialEq, Eq, Hash, Clone, Debug, Copy)]
pub enum ChannelEstablishment {
    NewChannel,
    ExistingChannel,
}

/// Marker trait.
pub trait Properties {}

#[derive(Debug)]
pub struct JobState<J> where J: Job {
    pub order: J::O,
    /// Used to manage the resources acquired for a job. It is set to Some(_) if there is an active job associated
    /// with the Channel, or None if the channel is just kept open for requests that may arrive in the future. The
    /// reason for using Optional (rather than just JS) is that this way, drop() will called on the JS as soon as we
    /// reset the state to None, so that acquired resources are released as soon as possible.
    pub job_resources: Option<J::JS>,
    pub tx: Sender<FlexoProgress>,
}

impl <J> JobState<J> where J: Job {
    /// Release all resources (e.g. opened files) that were required for this particular job.
    fn release_job_resources(&mut self) {
        self.job_resources = None
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug, Copy)]
pub struct CachedItem {
    pub complete_size: Option<u64>,
    pub cached_size: u64,
}

#[derive(PartialEq, Eq, Hash, Clone, Debug, Copy)]
pub enum OrderState {
    Cached(CachedItem),
    InProgress
}

/// The context in which a job is executed, including all stateful information required by the job.
/// This context is meant to be initialized once during the program's lifecycle.
pub struct JobContext<J> where J: Job, {
    providers: Arc<Mutex<Vec<J::P>>>,
    channels: Arc<Mutex<HashMap<J::P, J::C>>>,
    order_states: Arc<Mutex<HashMap<J::O, OrderState>>>,
    providers_in_use: Arc<Mutex<HashMap<J::P, i32>>>,
    panic_monitor: Vec<Arc<Mutex<i32>>>,
    provider_failures: Arc<Mutex<HashMap<J::P, i32>>>,
    pub properties: J::PR
}

pub struct ScheduledItem<J> where J: Job {
    pub join_handle: JoinHandle<JobOutcome<J>>,
    pub rx: Receiver<FlexoMessage<J::P>>,
    pub rx_progress: Receiver<FlexoProgress>,
}

pub enum ScheduleOutcome<J> where J: Job {
    /// The order is already in progress, no new order was scheduled.
    AlreadyInProgress,
    /// The order has to be fetched from a provider.
    Scheduled(ScheduledItem<J>),
    /// The order is already available in the cache.
    Cached,
    /// the order cannot be served from cache
    Uncacheable(J::P),
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum FlexoMessage <P> {
    ProviderSelected(P),
    ChannelEstablished(ChannelEstablishment),
    OrderError,
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum FlexoProgress {
    /// The job cannot be completed because the requested order is not available.
    Unavailable,
    JobSize(u64),
    Progress(u64),
    Completed,
    OrderError,
}


impl <J> JobContext<J> where J: Job {
    pub fn new(initial_providers: Vec<J::P>, properties: J::PR) -> Self {
        let providers: Arc<Mutex<Vec<J::P>>> = Arc::new(Mutex::new(initial_providers));
        let channels: Arc<Mutex<HashMap<J::P, J::C>>> = Arc::new(Mutex::new(HashMap::new()));
        let order_states: Arc<Mutex<HashMap<J::O, OrderState>>> = Arc::new(Mutex::new(J::initialize_cache(properties.clone())));
        let providers_in_use: Arc<Mutex<HashMap<J::P, i32>>> = Arc::new(Mutex::new(HashMap::new()));
        let provider_records: Arc<Mutex<HashMap<J::P, i32>>> = Arc::new(Mutex::new(HashMap::new()));
        let thread_mutexes: Vec<Arc<Mutex<i32>>> = Vec::new();
        Self {
            providers,
            channels,
            order_states,
            provider_failures: provider_records,
            providers_in_use,
            panic_monitor: thread_mutexes,
            properties,
        }
    }

    fn best_provider(&self, order: &J::O) -> J::P {
        match order.custom_provider() {
            None => {
                // no custom provider is required to fulfil this order: We can just choose the best provider
                // among all available providers.
                // Providers are assumed to be sorted in ascending order from best to worst.
                let providers: Vec<J::P> = self.providers.lock().unwrap().to_vec();
                providers[0].clone()
            }
            Some(p) => {
                // This is a "special order" that needs to be served by a custom provider.
                // Speaking in Arch Linux terminology: This is a request that must be served
                // from a custom repository / unofficial repository.
                p
            }
        }
    }

    /// Schedule the order, or return info on why scheduling this order is not possible or not necessary.
    pub fn try_schedule(&mut self, order: J::O, resume_from: Option<u64>) -> ScheduleOutcome<J> {
        if !order.is_cacheable() {
            return ScheduleOutcome::Uncacheable(self.best_provider(&order));
        }
        let resume_from = resume_from.unwrap_or(0);
        warn!(">>> check if order {:?} is cached", &order);
        let cached_size: u64 = {
            let mut order_states = self.order_states.lock().unwrap();
            let cached_size = match order_states.get(&order) {
                None if resume_from > 0 => {
                    // Cannot serve this order from cache: See issue #7
                    error!(">>> 1");
                    return ScheduleOutcome::Uncacheable(self.best_provider(&order));
                },
                None => 0, // TODO this right here.
                Some(OrderState::Cached(CachedItem { cached_size, .. } )) if cached_size < &resume_from => {
                    error!(">>> 3");
                    // Cannot serve this order from cache: See issue #7
                    return ScheduleOutcome::Uncacheable(self.best_provider(&order));
                },
                Some(OrderState::Cached(CachedItem { complete_size: Some(c), cached_size } )) if c == cached_size => {
                    error!(">>> 4");
                    return ScheduleOutcome::Cached;
                },
                Some(OrderState::Cached(CachedItem { cached_size, .. } )) => {
                    error!(">>> 5");
                    *cached_size
                },
                Some(OrderState::InProgress) => {
                    error!(">>> 6");
                    debug!("order {:?} already in progress: nothing to do.", &order);
                    return ScheduleOutcome::AlreadyInProgress;
                }
            };
            order_states.insert(order.clone(), OrderState::InProgress);
            cached_size
        };
        self.schedule(order, cached_size)
    }

    /// Schedules the job so that the order will be fetched from the provider.
    fn schedule(&mut self, order: J::O, cached_size: u64) -> ScheduleOutcome<J> {
        let mutex = Arc::new(Mutex::new(0));
        let mutex_cloned = Arc::clone(&mutex);
        self.panic_monitor = self.panic_monitor.drain(..).filter(|mutex| {
            match mutex.try_lock() {
                Ok(_) => {
                    false
                },
                Err(TryLockError::WouldBlock) => {
                    true
                },
                Err(TryLockError::Poisoned(_)) => {
                    panic!("Cannot continue: A previously run thread has panicked.")
                },
            }
        }).collect();
        self.panic_monitor.push(mutex);

        let (tx, rx) = unbounded::<FlexoMessage<J::P>>();
        let (tx_progress, rx_progress) = unbounded::<FlexoProgress>();
        let channels_cloned = Arc::clone(&self.channels);
        let providers_cloned: Vec<J::P> = self.providers.lock().unwrap().clone();
        let provider_failures_cloned = Arc::clone(&self.provider_failures);
        let providers_in_use_cloned = Arc::clone(&self.providers_in_use);
        let order_states = Arc::clone(&self.order_states);
        let order_cloned = order.clone();
        let properties = self.properties.clone();

        let mut provider_stats = ProvidersWithStats {
            providers: providers_cloned,
            provider_failures: provider_failures_cloned,
            provider_current_usages: providers_in_use_cloned,
        };
        let t = thread::spawn(move || {
            let _lock = mutex_cloned.lock().unwrap();
            let order: <J as Job>::O = order.clone();
            let result = order.try_until_success(
                &mut provider_stats,
                channels_cloned.clone(),
                tx,
                tx_progress,
                properties,
                cached_size,
            );
            order_states.lock().unwrap().remove(&order_cloned);
            match result {
                JobResult::Complete(mut complete_job) => {
                    complete_job.channel.job_state().release_job_resources();
                    let mut channels_cloned = channels_cloned.lock().unwrap();
                    channels_cloned.insert(complete_job.provider.clone(), complete_job.channel);
                    let cached_item = CachedItem {
                        complete_size: Some(complete_job.size as u64),
                        cached_size: complete_job.size as u64,
                    };
                    warn!(">>> mark order {:?} as cached", &order_cloned);
                    order_states.lock().unwrap().insert(order_cloned.clone(), OrderState::Cached(cached_item));
                    JobOutcome::Success(complete_job.provider.clone())
                }
                JobResult::Partial(JobPartiallyCompleted { mut channel, .. }) => {
                    channel.job_state().release_job_resources();
                    let provider_failures = provider_stats.provider_failures.lock().unwrap().clone();
                    JobOutcome::Error(provider_failures)
                }
                JobResult::Error(JobTerminated { mut channel, .. } ) => {
                    channel.job_state().release_job_resources();
                    let provider_failures = provider_stats.provider_failures.lock().unwrap().clone();
                    JobOutcome::Error(provider_failures)
                }
                JobResult::Unavailable(mut channel) => {
                    info!("The given order was unavailable for all providers.");
                    channel.job_state().release_job_resources();
                    let provider_failures = provider_stats.provider_failures.lock().unwrap().clone();
                    JobOutcome::Error(provider_failures)
                }
                JobResult::ClientError => {
                    let provider_failures = provider_stats.provider_failures.lock().unwrap().clone();
                    JobOutcome::Error(provider_failures)
                }
                JobResult::UnexpectedInternalError => {
                    let provider_failures = provider_stats.provider_failures.lock().unwrap().clone();
                    JobOutcome::Error(provider_failures)
                }
            }
        });

        ScheduleOutcome::Scheduled(ScheduledItem { join_handle: t, rx, rx_progress })
    }
}

#[test]
fn test_no_failures_preferred() {
    let s1 = DynamicScore {
        num_failures: 2,
        num_current_usages: 23,
        initial_score: 0,
    };
    let s2 = DynamicScore {
        num_failures: 0,
        num_current_usages: 0,
        initial_score: -1,
    };
    assert!(s2 < s1);
}

#[test]
fn test_initial_score_lower_is_better() {
    let s1 = DynamicScore {
        num_failures: 0,
        num_current_usages: 0,
        initial_score: 0,
    };
    let s2 = DynamicScore {
        num_failures: 0,
        num_current_usages: 0,
        initial_score: -1,
    };
    assert!(s2 < s1);
}

