use thiserror::Error;

/// An error returned by devondb operations.
#[derive(Debug, Error)]
pub enum DevonError {
    /// An operating-system I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Bytes read from persistent storage are invalid or corrupted.
    #[error("corrupt data: {context}")]
    Corrupt {
        /// Details about the invalid bytes.
        context: String,
    },

    /// A database file requires a newer devondb reader.
    #[error(
        "database file version {file_version} requires reader version {min_reader_version}, but this devondb supports version {supported}; upgrade devondb to open this file"
    )]
    VersionMismatch {
        /// The format version that wrote the database file.
        file_version: u32,
        /// The oldest reader version capable of opening the file.
        min_reader_version: u32,
        /// The newest format version supported by this devondb binary.
        supported: u32,
    },

    /// A caller supplied an invalid argument.
    #[error("invalid argument: {context}")]
    InvalidArgument {
        /// Details about the invalid argument.
        context: String,
    },

    /// A requested database object could not be found.
    #[error("not found: {what}")]
    NotFound {
        /// The object that could not be found.
        what: String,
    },

    /// A concurrent transaction committed a conflicting write first.
    #[error("transaction conflict: {context}")]
    TransactionConflict {
        /// Which write conflicted and the LSN of the winning commit.
        context: String,
    },

    /// The operation cannot proceed within the configured memory_limit.
    #[error("memory budget exceeded: {context}")]
    BudgetExceeded {
        /// The charging category, requested bytes, charged bytes, and limit.
        context: String,
    },

    /// The database file is readable but writes are disabled — it enables a
    /// read-safe feature this build does not fully support
    /// (`docs/FORMAT.md` § Feature flag registry).
    #[error("read-only: {context}")]
    ReadOnly {
        /// Which feature forced read-only mode and what was refused.
        context: String,
    },

    /// A cross-process lock is held by another handle; acquisition is
    /// nonblocking by law (`docs/MULTIPROCESS.md` invariant 2), so the
    /// contended caller gets this error instead of waiting.
    #[error("busy: {context}")]
    Busy {
        /// Which lock was contended and the role/path that was refused.
        context: String,
    },
}

/// A result whose error type is [`DevonError`].
pub type DevonResult<T> = Result<T, DevonError>;

#[cfg(test)]
mod tests {
    use super::DevonError;
    use std::io::ErrorKind;

    #[test]
    fn version_mismatch_display_names_all_versions_and_upgrade_path() {
        let error = DevonError::VersionMismatch {
            file_version: 12,
            min_reader_version: 10,
            supported: 7,
        };

        let message = error.to_string();
        assert!(message.contains("12"));
        assert!(message.contains("10"));
        assert!(message.contains('7'));
        assert!(message.contains("upgrade devondb"));
    }

    #[test]
    fn io_error_converts_to_devon_error() {
        let io_error = std::io::Error::new(ErrorKind::PermissionDenied, "access denied");
        let error = DevonError::from(io_error);

        match error {
            DevonError::Io(source) => assert_eq!(source.kind(), ErrorKind::PermissionDenied),
            other => panic!("expected I/O error, got {other}"),
        }
    }
}
