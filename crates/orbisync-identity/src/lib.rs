//! Local authentication, sessions, tokens, users, roles and permissions.
//!
//! Concrete persistence and telemetry adapters are accessed only through
//! `orbisync-application` ports.

pub mod admin;
pub mod csv_import;
pub mod ephemeral;
pub mod external;
pub mod login;
pub mod password;
pub mod rbac;
pub mod refresh;
pub mod service;
pub mod session_issuer;
pub mod ticket;
pub mod token;

pub use admin::{
    DynClock, DynIdentityAdministrationStore, IdentityAdministrationService,
    PasswordIdempotencyContext,
};
pub use csv_import::{
    CsvImportError, CsvUserImport, CsvUserRow, DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS, parse_user_csv,
    parse_user_csv_with_limits,
};
pub use ephemeral::{EphemeralMethodPolicy, EphemeralSubjectService};
pub use external::{ExternalAuthService, JwtIdentityProvider};
pub use login::LoginService;
pub use password::{PasswordError, PasswordPolicy, PasswordService};
pub use rbac::{AuthorizationDecision, RbacAuthorizer};
pub use refresh::{RefreshError, RefreshTokenFamily, RotationOutcome};
pub use service::{AuthenticationService, AuthenticationSubject};
pub use session_issuer::{IssuableSubject, IssuedTokens, SessionIssuer};
pub use ticket::ConnectionTicket;
pub use token::{AccessTokenClaims, AccessTokenError, AccessTokenService, VerificationKey};
