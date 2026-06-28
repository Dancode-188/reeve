pub struct AsciiMode(bool);

impl AsciiMode {
    pub fn new(enabled: bool) -> Self {
        Self(enabled)
    }

    pub fn enabled(&self) -> bool {
        self.0
    }

    pub fn tree_open(&self) -> &'static str {
        if self.0 { "v " } else { "▾ " }
    }

    pub fn tree_closed(&self) -> &'static str {
        if self.0 { "> " } else { "▸ " }
    }

    pub fn tree_pipe(&self) -> &'static str {
        if self.0 { "|  " } else { "│  " }
    }

    pub fn tree_tee(&self) -> &'static str {
        if self.0 { "|- " } else { "├─ " }
    }

    pub fn tree_elbow(&self) -> &'static str {
        if self.0 { "`- " } else { "└─ " }
    }

    pub fn cursor(&self) -> &'static str {
        if self.0 { "_" } else { "▌" }
    }
}
