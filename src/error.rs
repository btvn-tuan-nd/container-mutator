use std::fmt;

/// A verified candidate violates the release contract; retrying its digest cannot help.
#[derive(Debug)]
pub struct InvalidRelease(pub String);

impl fmt::Display for InvalidRelease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InvalidRelease {}

pub fn invalid(message: impl Into<String>) -> anyhow::Error {
    InvalidRelease(message.into()).into()
}
