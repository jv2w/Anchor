//! Why the commands registered on a [`ServerBuilder`](crate::ServerBuilder) cannot be built, or
//! why a soft enum cannot be set.
//!
//! These errors are never shown to a command's sender; what a sender sees is
//! [`command::Error`](crate::command::Error).

use std::fmt;

/// Why the registered commands and permissions cannot be built, or a soft enum cannot be set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A command or alias name is empty, too long (64 bytes) or holds spaces, control characters
    /// or `/`.
    InvalidName {
        /// The name.
        name: String,
    },
    /// Two registered commands share a name or alias.
    Duplicate {
        /// The name.
        name: String,
    },
    /// [`ServerBuilder::without_command`](crate::ServerBuilder::without_command) named no
    /// command of Anchor's own (an alias or an unknown name).
    UnknownCommand {
        /// The name.
        name: String,
    },
    /// An overload breaks a rule of [`Command::overload`](crate::command::Command::overload), the
    /// command has none, or it names more than 16 permissions.
    InvalidOverload {
        /// The command's name.
        command: String,
        /// What is wrong.
        reason: &'static str,
    },
    /// A command's description is longer than a client takes (1,000 bytes).
    DescriptionTooLong {
        /// The command's name.
        command: String,
    },
    /// Two permissions with the same name have different defaults.
    ConflictingPermission {
        /// The permission's name.
        name: &'static str,
    },
    /// [`ServerBuilder::permission_default`](crate::ServerBuilder::permission_default) named a
    /// permission no command and no
    /// [`ServerBuilder::permission`](crate::ServerBuilder::permission) registers.
    UnknownPermission {
        /// The permission's name.
        name: &'static str,
    },
    /// Two enum parameters with the same type name have different values.
    ConflictingEnum {
        /// The enum's type name.
        name: String,
    },
    /// [`Context::set_soft_enum`](crate::Context::set_soft_enum) named no soft enum type of a
    /// registered command, or values it refuses (see its limits).
    InvalidSoftEnum {
        /// The enum's type name.
        name: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => write!(f, "invalid command name {name:?}"),
            Self::Duplicate { name } => write!(f, "two commands are named {name:?}"),
            Self::UnknownCommand { name } => {
                write!(f, "no command of Anchor's is named {name:?}")
            }
            Self::InvalidOverload { command, reason } => {
                write!(f, "invalid overload of command {command:?}: {reason}")
            }
            Self::DescriptionTooLong { command } => {
                write!(
                    f,
                    "the description of command {command:?} is longer than a client takes"
                )
            }
            Self::ConflictingPermission { name } => {
                write!(f, "permission {name:?} is registered with two defaults")
            }
            Self::UnknownPermission { name } => {
                write!(f, "permission {name:?} has a default but is not registered")
            }
            Self::ConflictingEnum { name } => {
                write!(f, "enum type {name:?} is registered with two value lists")
            }
            Self::InvalidSoftEnum { name } => {
                write!(f, "no soft enum type {name:?} takes these values")
            }
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: std::error::Error + Send + Sync>() {}

    #[test]
    fn a_registration_error_tells_its_cause_and_is_a_plain_error() {
        assert_send_sync::<Error>();
        assert_eq!(
            Error::Duplicate {
                name: "give".to_owned()
            }
            .to_string(),
            "two commands are named \"give\""
        );
    }
}
