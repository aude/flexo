use std::path::{Path, PathBuf};

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct StrPath {
    path_buf: PathBuf,
    inner: String,
}

impl std::convert::AsRef<std::path::Path> for StrPath {
    fn as_ref(&self) -> &Path {
        self.path_buf.as_path()
    }
}

impl StrPath {
    pub fn new(s: String) -> Self {
        let s = if s.starts_with("/") {
            warn!(">>> skip the leading / from {:?}", &s);
            s[1..].to_owned()
        } else {
            s
        };
        warn!(">>> s is: {:?}", s);
        let p = StrPath {
            path_buf: Path::new(&s).to_path_buf(),
            inner: s,
        };
        warn!(">>> p is: {:?}", p);
        p
    }

    pub fn from_path_buf(path_buf: PathBuf) -> Option<Self> {
        let inner = path_buf.to_str().unwrap().to_owned();
        Some(StrPath::new(inner))
    }

    pub fn to_str(&self) -> &str {
        &self.inner
    }
}

