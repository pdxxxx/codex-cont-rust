use crate::config::Config;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

impl Level {
    pub fn from_str(level: &str) -> Self {
        match level.trim().to_ascii_lowercase().as_str() {
            "off" => Self::Off,
            "error" => Self::Error,
            "warn" => Self::Warn,
            "info" => Self::Info,
            "debug" => Self::Debug,
            _ => Self::Info,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    fn allows(self, level: Self) -> bool {
        self != Self::Off && self >= level
    }
}

pub fn configured_level(cfg: &Config) -> Level {
    Level::from_str(&cfg.log.level)
}

pub fn configured_level_name(cfg: &Config) -> &'static str {
    configured_level(cfg).as_str()
}

pub fn error(cfg: &Config, message: impl AsRef<str>) {
    emit(cfg, Level::Error, message);
}

pub fn warn(cfg: &Config, message: impl AsRef<str>) {
    emit(cfg, Level::Warn, message);
}

pub fn info(cfg: &Config, message: impl AsRef<str>) {
    emit(cfg, Level::Info, message);
}

pub fn debug(cfg: &Config, message: impl AsRef<str>) {
    emit(cfg, Level::Debug, message);
}

fn emit(cfg: &Config, level: Level, message: impl AsRef<str>) {
    if !configured_level(cfg).allows(level) {
        return;
    }
    let line = format!("[{}] {}", level.as_str(), message.as_ref());
    if level <= Level::Warn {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::Level;

    #[test]
    fn parses_log_levels() {
        assert_eq!(Level::from_str("off"), Level::Off);
        assert_eq!(Level::from_str("error"), Level::Error);
        assert_eq!(Level::from_str("warn"), Level::Warn);
        assert_eq!(Level::from_str("info"), Level::Info);
        assert_eq!(Level::from_str("debug"), Level::Debug);
        assert_eq!(Level::from_str("wat"), Level::Info);
    }
}
