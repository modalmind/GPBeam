use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout { Flat }

#[derive(Debug, Clone)]
pub struct Config {
    pub dest_root: PathBuf,
    pub filename_template: String,
    pub include_proxies: bool,
    pub include_thumbnails: bool,
    pub layout: Layout,
    pub verify: bool,
    pub space_headroom: u64,
}

impl Config {
    pub fn new(dest_root: PathBuf) -> Self {
        Config {
            dest_root,
            filename_template: "{date}_{original}".into(),
            include_proxies: false,
            include_thumbnails: false,
            layout: Layout::Flat,
            verify: true,
            space_headroom: 1024 * 1024 * 1024, // 1 GiB
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_match_spec() {
        let c = Config::new(PathBuf::from("/tmp/dest"));
        assert_eq!(c.filename_template, "{date}_{original}");
        assert!(!c.include_proxies);
        assert!(!c.include_thumbnails);
        assert_eq!(c.layout, Layout::Flat);
        assert!(c.verify);
        assert_eq!(c.space_headroom, 1024 * 1024 * 1024);
    }
}
