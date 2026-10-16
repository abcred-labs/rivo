#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SidebarView {
    #[default]
    Tools,
    Trace,
    Evidence,
    History,
    Errors,
}

impl SidebarView {
    pub fn next(self) -> Self {
        match self {
            Self::Tools => Self::Trace,
            Self::Trace => Self::Evidence,
            Self::Evidence => Self::History,
            Self::History => Self::Errors,
            Self::Errors => Self::Tools,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Tools => Self::Errors,
            Self::Trace => Self::Tools,
            Self::Evidence => Self::Trace,
            Self::History => Self::Evidence,
            Self::Errors => Self::History,
        }
    }
}
