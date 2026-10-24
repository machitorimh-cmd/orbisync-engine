//! Compound audit search/export through HTTP and real PostgreSQL; UUID-scoped fixtures.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod common;

use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Request, StatusCode},
};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityRepository, pagination::CursorCodec};
use orbisync_domain::{AuthSession, AuthSessionId, Timestamp, UserId};
use orbisync_identity::token::AccessTokenService;
use orbisync_storage_postgres::{PgIdentityQueryStore, PgIdentityRepository};
use orbisync_testkit::FixedClock;
use orbisync_transport_http::{HttpState, router};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use tower::ServiceExt as _;
use uuid::Uuid;

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

struct Fixture {
    pool: PgPool,
    state: HttpState,
    admin: UserId,
    other: UserId,
    token: String,
    other_token: String,
    base: OffsetDateTime,
    scope: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let pool = common::pool_or_skip().await?;
        let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let timestamp = Timestamp::from_offset_date_time(now);
        let repo = Arc::new(PgIdentityRepository::new(pool.clone()));
        let mut actors = Vec::new();
        for _ in 0..2 {
            let user = UserId::generate();
            sqlx::query("INSERT INTO users(id,login_id,display_name,status,must_change_password,revision,created_at,updated_at) VALUES($1,$2,'Audit test','active',false,1,$3,$3)")
                .bind(user.as_uuid()).bind(format!("audit-test-{user}")).bind(now).execute(&pool).await.unwrap();
            let id = AuthSessionId::generate();
            let session = AuthSession::new(
                id,
                user,
                timestamp,
                Timestamp::from_offset_date_time(now + Duration::hours(1)),
            )
            .unwrap();
            repo.save_session(&session).await.unwrap();
            actors.push((user, id));
        }
        let admin = actors[0].0;
        let other = actors[1].0;
        let role = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO permissions(name) VALUES('admin.audit.read') ON CONFLICT DO NOTHING",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO roles(id,name,revision) VALUES($1,$2,1)")
            .bind(role)
            .bind(format!("audit-reader-{role}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO role_permissions(role_id,permission_name) VALUES($1,'admin.audit.read')",
        )
        .bind(role)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_roles(user_id,role_id) VALUES($1,$2)")
            .bind(admin.as_uuid())
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
        let tokens = Arc::new(
            AccessTokenService::from_ed25519_pem(
                PRIVATE_PEM,
                PUBLIC_PEM,
                "orbisync",
                "orbisync-api",
                "test",
            )
            .unwrap(),
        );
        let token = tokens
            .issue(admin, actors[0].1, timestamp)
            .unwrap()
            .expose_secret()
            .to_owned();
        let other_token = tokens
            .issue(other, actors[1].1, timestamp)
            .unwrap()
            .expose_secret()
            .to_owned();
        let query = Arc::new(PgIdentityQueryStore::with_codec(
            pool.clone(),
            CursorCodec::new(b"audit-search-independent-pagination-key".to_vec()).unwrap(),
        ));
        let state = HttpState::new(
            vec![],
            "orbisync",
            "test",
            1,
            Arc::new(FixedClock::new(timestamp)),
            900,
            2592000,
            b"audit-refresh-test-key".to_vec(),
        )
        .with_token_service(tokens)
        .with_identity_repository(repo)
        .with_audit_query(query)
        .with_allowed_origins(vec![HeaderValue::from_static("https://audit-ui.test")]);
        Some(Self {
            pool,
            state,
            admin,
            other,
            token,
            other_token,
            base: now.replace_nanosecond(123_000_000).unwrap(),
            scope: format!("audit-test-{}", Uuid::now_v7()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn event(
        &self,
        at: OffsetDateTime,
        actor: UserId,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        request_id: &str,
        metadata: Value,
    ) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO audit_events(id,occurred_at,actor_user_id,action,target_type,target_id,request_id,result,metadata) VALUES($1,$2,$3,$4,$5,$6,$7,'success',$8)")
            .bind(id).bind(at).bind(actor.as_uuid()).bind(action).bind(resource_type).bind(resource_id).bind(request_id).bind(metadata)
            .execute(&self.pool).await.unwrap();
        id
    }

    async fn get(&self, token: Option<&str>, path: &str) -> (StatusCode, HeaderMap, String) {
        let mut request = Request::builder()
            .uri(path)
            .header("origin", "https://audit-ui.test");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router(self.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

fn path(endpoint: &str, fields: &[(&str, String)], cursor: Option<&str>, limit: u16) -> String {
    let mut url = reqwest::Url::parse(&format!("http://audit.test{endpoint}")).unwrap();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("limit", &limit.to_string());
        for (name, value) in fields {
            query.append_pair(name, value);
        }
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor);
        }
    }
    format!("{}?{}", url.path(), url.query().unwrap())
}

fn json_page(status: StatusCode, text: &str) -> Value {
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn composite_audit_search_paginates_microseconds_and_timestamp_ties_without_loss() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let action = format!("{}.compound", f.scope);
    let resource_type = format!("{}.resource", f.scope);
    let resource_id = "=SUM(1,2)";
    let request_id = format!("req_{}", Uuid::now_v7());
    let from = f.base + Duration::microseconds(50);
    let to = f.base + Duration::microseconds(950);
    let fields = vec![
        ("actor_id", f.admin.to_string()),
        ("action", action.clone()),
        ("resource_type", resource_type.clone()),
        ("resource_id", resource_id.to_owned()),
        ("request_id", request_id.clone()),
        ("from", from.format(&Rfc3339).unwrap()),
        ("to", to.format(&Rfc3339).unwrap()),
    ];
    let mut expected = Vec::new();
    for micros in [900, 900, 700, 100, 100] {
        let at = f.base + Duration::microseconds(micros);
        expected.push((
            at,
            f.event(
                at,
                f.admin,
                &action,
                &resource_type,
                resource_id,
                &request_id,
                json!({"safe":true}),
            )
            .await,
        ));
    }
    // Each individually changed predicate must exclude its own distractor.
    for (actor, event_action, kind, target, request, at) in [
        (
            f.other,
            action.as_str(),
            resource_type.as_str(),
            resource_id,
            request_id.as_str(),
            f.base + Duration::microseconds(500),
        ),
        (
            f.admin,
            "different-action",
            resource_type.as_str(),
            resource_id,
            request_id.as_str(),
            f.base + Duration::microseconds(500),
        ),
        (
            f.admin,
            action.as_str(),
            "different-type",
            resource_id,
            request_id.as_str(),
            f.base + Duration::microseconds(500),
        ),
        (
            f.admin,
            action.as_str(),
            resource_type.as_str(),
            "different-target",
            request_id.as_str(),
            f.base + Duration::microseconds(500),
        ),
        (
            f.admin,
            action.as_str(),
            resource_type.as_str(),
            resource_id,
            "different-request",
            f.base + Duration::microseconds(500),
        ),
        (
            f.admin,
            action.as_str(),
            resource_type.as_str(),
            resource_id,
            request_id.as_str(),
            from - Duration::microseconds(1),
        ),
        (
            f.admin,
            action.as_str(),
            resource_type.as_str(),
            resource_id,
            request_id.as_str(),
            to + Duration::microseconds(1),
        ),
    ] {
        f.event(
            at,
            actor,
            event_action,
            kind,
            target,
            request,
            json!({"distractor":true}),
        )
        .await;
    }
    expected.sort_by(|a, b| b.cmp(a));
    let mut actual = Vec::new();
    let mut cursor = None;
    let mut first_cursor = None;
    for _ in 0..4 {
        let (status, headers, text) = f
            .get(
                Some(&f.token),
                &path("/v1/audit-events", &fields, cursor.as_deref(), 2),
            )
            .await;
        assert_eq!(headers["cache-control"], "no-store");
        let page = json_page(status, &text);
        actual.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| Uuid::parse_str(item["audit_id"].as_str().unwrap()).unwrap()),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if first_cursor.is_none() {
            first_cursor.clone_from(&cursor);
        }
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        actual,
        expected.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
    );
    assert!(cursor.is_none(), "pagination must terminate");
    let cursor = first_cursor.unwrap();
    for index in 0..fields.len() {
        let mut changed = fields.clone();
        changed[index].1 = match changed[index].0 {
            "actor_id" => f.other.to_string(),
            "from" => (from + Duration::nanoseconds(1)).format(&Rfc3339).unwrap(),
            "to" => (to + Duration::nanoseconds(1)).format(&Rfc3339).unwrap(),
            _ => format!("{}-changed", changed[index].1),
        };
        for endpoint in ["/v1/audit-events", "/v1/audit-events/export"] {
            assert_eq!(
                f.get(Some(&f.token), &path(endpoint, &changed, Some(&cursor), 2))
                    .await
                    .0,
                StatusCode::BAD_REQUEST,
                "cursor must bind {} exactly, including submillisecond bounds",
                changed[index].0
            );
        }
    }
}

#[tokio::test]
async fn audit_exports_require_authentication_and_the_audit_read_permission() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    for endpoint in [
        "/v1/audit-events",
        "/v1/audit-events/export?format=json",
        "/v1/audit-events/export?format=csv",
    ] {
        assert_eq!(f.get(None, endpoint).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            f.get(Some(&f.other_token), endpoint).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.get(Some("invalid-token"), endpoint).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}

fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut field = String::new();
    let mut row = Vec::new();
    let mut rows = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\r' if !quoted && chars.peek() == Some(&'\n') => {
                chars.next();
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    assert!(!quoted);
    assert!(field.is_empty());
    assert!(row.is_empty());
    rows
}

#[tokio::test]
async fn audit_json_csv_and_detail_redact_nested_secrets_and_neutralize_formula_cells() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let action = format!("{}.export", f.scope);
    let metadata = json!({"safe":"quote,\"line\"\nnext", "Password":"private-password",
        "nested":[{"access_token":"private-access", "Refresh-Token":"private-refresh", "client_secret":"private-client", "source_ip":"192.0.2.10", "safe_count":3},
            {"deeper":{"password_hash":"private-hash", "secret":"private-secret", "authorization":"private-auth", "cookie":"private-cookie", "remote_ip":"192.0.2.11", "ip_address":"192.0.2.12"}}]});
    let targets = ["=SUM(1,2)", " \t=SUM(1,2)", "safe,\"quoted\"\nnewline"];
    let mut ids = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let id = f
            .event(
                f.base + Duration::microseconds(i64::try_from(index).unwrap()),
                f.admin,
                &action,
                "entity",
                target,
                &f.scope,
                metadata.clone(),
            )
            .await;
        sqlx::query("INSERT INTO audit_source_ips(audit_event_id,source_ip,created_at) VALUES($1,'192.0.2.99',$2)")
            .bind(id).bind(f.base - Duration::days(2)).execute(&f.pool).await.unwrap();
        ids.push(id);
    }
    let filters = vec![("action", action)];
    let (status, _, list) = f
        .get(
            Some(&f.token),
            &path("/v1/audit-events", &filters, None, 200),
        )
        .await;
    assert_eq!(
        json_page(status, &list)["items"].as_array().unwrap().len(),
        3
    );
    for id in &ids {
        let (status, headers, text) = f
            .get(Some(&f.token), &format!("/v1/audit-events/{id}"))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        assert_redacted(&text);
    }
    let mut json_filters = filters.clone();
    json_filters.push(("format", "json".into()));
    let (status, headers, text) = f
        .get(
            Some(&f.token),
            &path("/v1/audit-events/export", &json_filters, None, 2),
        )
        .await;
    let page = json_page(status, &text);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert!(page["next_cursor"].is_string());
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"audit-events.json\""
    );
    assert_redacted(&list);
    assert_redacted(&text);
    let mut csv_filters = filters;
    csv_filters.push(("format", "csv".into()));
    let (status, headers, csv) = f
        .get(
            Some(&f.token),
            &path("/v1/audit-events/export", &csv_filters, None, 2),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/csv; charset=utf-8");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"audit-events.csv\""
    );
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        headers["access-control-allow-origin"],
        "https://audit-ui.test"
    );
    let exposed = headers["access-control-expose-headers"]
        .to_str()
        .unwrap()
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>();
    for header in ["x-next-cursor", "content-disposition"] {
        assert!(
            exposed.contains(&header),
            "browser consumers must be able to read {header}"
        );
    }
    assert_redacted(&csv);
    let cursor = headers["x-next-cursor"].to_str().unwrap();
    let mut rows = csv_rows(&csv);
    assert_eq!(rows.len(), 3);
    let (status, headers, second) = f
        .get(
            Some(&f.token),
            &path("/v1/audit-events/export", &csv_filters, Some(cursor), 2),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("x-next-cursor"));
    rows.extend(csv_rows(&second).into_iter().skip(1));
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|row| row.len() == 11));
    let target_cells = rows
        .iter()
        .skip(1)
        .map(|row| row[6].as_str())
        .collect::<Vec<_>>();
    assert!(target_cells.contains(&"'=SUM(1,2)"));
    assert!(target_cells.contains(&"' \t=SUM(1,2)"));
    assert!(target_cells.contains(&"safe,\"quoted\"\nnewline"));
    for row in rows.iter().skip(1) {
        let details: Value = serde_json::from_str(&row[10]).unwrap();
        assert_eq!(details["safe"], "quote,\"line\"\nnext");
        assert_eq!(details["nested"][0]["safe_count"], 3);
    }
    // Retention sweeps are global. Remove only our disposable events; the FK
    // cascade also removes their expired source-IP rows before other targets run.
    sqlx::query("DELETE FROM audit_events WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&f.pool)
        .await
        .unwrap();
}

fn assert_redacted(text: &str) {
    for private in [
        "private-password",
        "private-access",
        "private-refresh",
        "private-client",
        "private-hash",
        "private-secret",
        "private-auth",
        "private-cookie",
        "192.0.2.",
    ] {
        assert!(
            !text.contains(private),
            "private metadata leaked: {private}"
        );
    }
}

#[tokio::test]
async fn audit_export_limits_are_bounded_and_continue_without_loading_all_records() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let action = format!("{}.bounds", f.scope);
    let ids = (0..201).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    sqlx::query("INSERT INTO audit_events(id,occurred_at,actor_user_id,action,target_type,target_id,request_id,result,metadata) SELECT id,$2,$3,$4,'entity','target',$5,'success','{}'::jsonb FROM unnest($1::uuid[]) AS id")
        .bind(&ids).bind(f.base).bind(f.admin.as_uuid()).bind(&action).bind(&f.scope).execute(&f.pool).await.unwrap();
    let filters = vec![("action", action), ("format", "json".into())];
    let (status, _, text) = f
        .get(
            Some(&f.token),
            &path("/v1/audit-events/export", &filters, None, 200),
        )
        .await;
    let first = json_page(status, &text);
    assert_eq!(first["items"].as_array().unwrap().len(), 200);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, _, text) = f
        .get(
            Some(&f.token),
            &path("/v1/audit-events/export", &filters, Some(cursor), 200),
        )
        .await;
    let second = json_page(status, &text);
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert!(second.get("next_cursor").is_none());
    let mut actual = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["items"].as_array().unwrap())
        .map(|item| Uuid::parse_str(item["audit_id"].as_str().unwrap()).unwrap())
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = ids;
    expected.sort();
    assert_eq!(actual, expected);
    for endpoint in ["/v1/audit-events", "/v1/audit-events/export"] {
        for format in ["json", "csv"] {
            for limit in [0, 201] {
                assert_eq!(
                    f.get(
                        Some(&f.token),
                        &path(endpoint, &[("format", format.into())], None, limit)
                    )
                    .await
                    .0,
                    StatusCode::BAD_REQUEST
                );
            }
        }
    }
    assert_eq!(
        f.get(Some(&f.token), "/v1/audit-events/export?format=zip")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}
