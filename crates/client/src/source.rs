#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
  Dsh,
  Pi,
  Qoder,
  Codex,
  OpenCode,
  ZCode,
  WorkBuddy,
}

impl Source {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Dsh => "dsh",
      Self::Pi => "pi",
      Self::Qoder => "qoder",
      Self::Codex => "codex",
      Self::OpenCode => "opencode",
      Self::ZCode => "zcode",
      Self::WorkBuddy => "workbuddy",
    }
  }
}
