use std::path::PathBuf;

const DEFAULT_SHELL: &str = "/bin/bash";

#[derive(Debug, Clone)]
pub struct BackendSettings {
    pub shell: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    /// KXS: lines of scrollback kept (alacritty's default is 10,000; each line costs `columns` cells
    /// twice over while the screen is copied for drawing, see `TerminalBackend::sync`).
    pub scrollback: usize,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            shell: DEFAULT_SHELL.to_string(),
            args: vec![],
            working_directory: None,
            scrollback: 10_000,
        }
    }
}
