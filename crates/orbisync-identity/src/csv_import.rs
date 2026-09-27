//! Strict, bounded CSV user-import parsing from ADR-016.

use std::collections::HashSet;

use orbisync_domain::LoginId;

/// Default maximum CSV payload size.
pub const DEFAULT_MAX_BYTES: usize = 1_048_576;
/// Default maximum CSV data rows.
pub const DEFAULT_MAX_ROWS: usize = 1_000;

/// A valid row ready for the user-creation use case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvUserRow {
    /// One-based source row, including the header as row one.
    pub row: usize,
    /// Validated login identifier.
    pub login_id: LoginId,
    /// Display name awaiting domain aggregate validation.
    pub display_name: String,
}

/// Per-row rejection that permits partial success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvImportError {
    /// One-based source row.
    pub row: usize,
    /// Stable public error code.
    pub code: &'static str,
}

/// Parsed import with independent accepted and rejected rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvUserImport {
    /// Rows eligible for creation.
    pub accepted: Vec<CsvUserRow>,
    /// Invalid or duplicate rows.
    pub rejected: Vec<CsvImportError>,
}

/// Parses UTF-8 `text/csv` with the exact `login_id,display_name` header.
///
/// Input is capped at 1 MiB and 1,000 data rows. Duplicate login IDs within
/// one file are rejected as `RESOURCE_CONFLICT` without discarding other rows.
///
/// # Errors
///
/// Returns a stable error when media type, UTF-8, header, size, or row count is invalid.
pub fn parse_user_csv(content_type: &str, bytes: &[u8]) -> Result<CsvUserImport, &'static str> {
    parse_user_csv_with_limits(content_type, bytes, DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS)
}

/// Parses CSV with deployment-provided size and row limits.
pub fn parse_user_csv_with_limits(
    content_type: &str,
    bytes: &[u8],
    max_bytes: usize,
    max_rows: usize,
) -> Result<CsvUserImport, &'static str> {
    if content_type != "text/csv" {
        return Err("UNSUPPORTED_MEDIA_TYPE");
    }
    if bytes.len() > max_bytes || std::str::from_utf8(bytes).is_err() {
        return Err("INVALID_CSV");
    }
    let mut reader = csv::ReaderBuilder::new().flexible(false).from_reader(bytes);
    let header = reader.headers().map_err(|_| "INVALID_CSV")?;
    if header.len() != 2
        || header.get(0) != Some("login_id")
        || header.get(1) != Some("display_name")
    {
        return Err("INVALID_CSV_HEADER");
    }
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    let mut seen = HashSet::new();
    for (index, record) in reader.records().enumerate() {
        if index >= max_rows {
            return Err("CSV_LIMIT_EXCEEDED");
        }
        let row = index + 2;
        let record = match record {
            Ok(value) => value,
            Err(_) => {
                rejected.push(CsvImportError {
                    row,
                    code: "INVALID_CSV_ROW",
                });
                continue;
            }
        };
        let (Some(login_text), Some(display_name)) = (record.get(0), record.get(1)) else {
            rejected.push(CsvImportError {
                row,
                code: "INVALID_CSV_ROW",
            });
            continue;
        };
        let Ok(login_id) = LoginId::new(login_text) else {
            rejected.push(CsvImportError {
                row,
                code: "INVALID_CSV_ROW",
            });
            continue;
        };
        // ADR-026: bulk import is a second account-creation path, so it has to
        // refuse the prefixes reserved for server-generated subjects too.
        // Rejecting only in the single-user endpoint would leave this one open.
        if login_id.is_reserved() {
            rejected.push(CsvImportError {
                row,
                code: "INVALID_CSV_ROW",
            });
            continue;
        }
        if !seen.insert(login_text.to_owned()) {
            rejected.push(CsvImportError {
                row,
                code: "RESOURCE_CONFLICT",
            });
            continue;
        }
        accepted.push(CsvUserRow {
            row,
            login_id,
            display_name: display_name.to_owned(),
        });
    }
    Ok(CsvUserImport { accepted, rejected })
}

#[cfg(test)]
mod tests {
    use super::parse_user_csv;

    #[test]
    fn duplicate_rows_are_partial_resource_conflicts() {
        let parsed = parse_user_csv(
            "text/csv",
            b"login_id,display_name\nada,Ada\nada,Duplicate\ngrace,Grace\n",
        )
        .expect("valid file");
        assert_eq!(parsed.accepted.len(), 2);
        assert_eq!(parsed.rejected.len(), 1);
        assert_eq!(parsed.rejected[0].code, "RESOURCE_CONFLICT");
    }

    #[test]
    fn media_type_header_and_limits_are_enforced() {
        assert!(parse_user_csv("application/json", b"").is_err());
        assert!(parse_user_csv("text/csv", b"login,display_name\nada,Ada\n").is_err());
        let oversized = vec![b'a'; 1_048_577];
        assert!(parse_user_csv("text/csv", &oversized).is_err());
    }
}

#[cfg(test)]
mod reserved_prefix_tests {
    use super::parse_user_csv;

    #[test]
    fn rows_claiming_a_reserved_login_prefix_are_rejected() {
        // Bulk import is a second account-creation path. Guarding only the
        // single-user endpoint would let an operator create an account that
        // collides with a generated subject through this one instead.
        let csv = "login_id,display_name\n\
                   guest:0192d43d-a18a-7fed-8123-0123456789ab,Impostor\n\
                   name:0192d43d-a18a-7fed-8123-0123456789ab,Impostor\n\
                   ext:0192d43d-a18a-7fed-8123-0123456789ab,Impostor\n\
                   ada,Ada\n";
        let import = parse_user_csv("text/csv", csv.as_bytes()).expect("csv parses");
        assert_eq!(
            import.accepted.len(),
            1,
            "only the unreserved login id may be accepted"
        );
        assert_eq!(import.accepted[0].login_id.as_str(), "ada");
        assert_eq!(import.rejected.len(), 3);
        assert!(
            import
                .rejected
                .iter()
                .all(|error| error.code == "INVALID_CSV_ROW")
        );
    }

    #[test]
    fn a_login_id_merely_starting_with_the_same_letters_is_accepted() {
        // "guestbook" is not the reserved "guest:" prefix, so it stays usable.
        let csv = "login_id,display_name\nguestbook,Guest Book\nnamed,Named\n";
        let import = parse_user_csv("text/csv", csv.as_bytes()).expect("csv parses");
        assert_eq!(import.accepted.len(), 2);
        assert!(import.rejected.is_empty());
    }
}
