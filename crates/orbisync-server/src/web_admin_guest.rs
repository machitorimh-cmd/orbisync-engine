// Guest participation is ordinary engine configuration, edited by an authenticated
// operator. The app still supplies its own engine URL and Instance ID.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestSettingsEdit {
    enabled: bool,
    allowed_worlds: Vec<String>,
}

fn edit_guest_config(
    config: &mut Config,
    table: &mut toml::Table,
    edit: &GuestSettingsEdit,
) -> WebResult<()> {
    let mut worlds = std::collections::BTreeSet::new();
    for raw in &edit.allowed_worlds {
        let id = Uuid::parse_str(raw)
            .ok()
            .and_then(|id| orbisync_domain::WorldId::new(id).ok())
            .ok_or_else(|| {
                bad("Guest participation requires valid World IDs, not Instance IDs.")
            })?;
        worlds.insert(id.to_string());
    }
    config.auth.guest.allowed_worlds = worlds.into_iter().collect();
    // Preserve local, external and name-only authentication and every existing
    // guest lifetime/prefix setting. Turning guest off retains its World choices.
    config.auth.methods.retain(|method| method != "guest");
    if edit.enabled {
        config.auth.methods.push("guest".to_owned());
        if config.auth.guest.role_names.is_empty() {
            config.auth.guest.role_names = vec!["Guest".to_owned()];
        }
    }
    let auth = table
        .entry("auth")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| bad("Invalid auth configuration section."))?;
    let strings = |values: &[String]| {
        toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect())
    };
    auth.insert("methods".into(), strings(&config.auth.methods));
    let guest = auth
        .entry("guest")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| bad("Invalid guest configuration section."))?;
    guest.insert(
        "allowed_worlds".into(),
        strings(&config.auth.guest.allowed_worlds),
    );
    guest.insert("role_names".into(), strings(&config.auth.guest.role_names));
    Ok(())
}

async fn validate_guest_setup(
    state: &WebState,
    install: &mut Installation,
    headers: &HeaderMap,
    config: &Config,
    replacement_database: &str,
) -> WebResult<()> {
    let env = install.env()?;
    let database_url = if replacement_database.is_empty() {
        env.get(&config.database.url_env)
            .ok_or_else(|| bad("Configured database is unavailable."))?
    } else {
        replacement_database.to_owned()
    };
    let pool = connect(&database_url).await?;
    let worlds: Vec<Uuid> = config
        .auth
        .guest
        .allowed_worlds
        .iter()
        .map(|raw| Uuid::parse_str(raw).map_err(|_| bad("Invalid guest World ID.")))
        .collect::<WebResult<_>>()?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM world_definitions WHERE id = ANY($1) AND status = 'active'",
    )
    .bind(&worlds)
    .fetch_one(&pool)
    .await
    .map_err(|_| bad("Could not verify guest World choices."))?;
    if count != i64::try_from(worlds.len()).unwrap_or(-1) {
        return Err(bad(
            "Select existing active Worlds for guest participation.",
        ));
    }
    // New installations get a join-only role through the ordinary RBAC-protected
    // roles API. Never replace a role or grant administrative rights to guests.
    if config.auth.guest.role_names == ["Guest"] {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM roles WHERE name = 'Guest')")
                .fetch_one(&pool)
                .await
                .map_err(|_| bad("Could not verify the Guest role."))?;
        if !exists {
            if !replacement_database.is_empty() {
                return Err(bad(
                    "Prepare a Guest role with world.instance.read in the replacement database first.",
                ));
            }
            let url = verified_engine_url(state, install).await?;
            let token = headers
                .get("authorization")
                .ok_or_else(|| bad("Administrator login required."))?;
            let response = state.client.post(format!("{url}/v1/roles"))
                .header("authorization", token)
                .header("idempotency-key", Uuid::now_v7().to_string())
                .json(&json!({"name":"Guest","description":"Guest participation","permissions":["world.instance.read"]}))
                .send().await.map_err(|_| bad("Could not prepare the Guest role."))?;
            if !response.status().is_success() && response.status() != StatusCode::CONFLICT {
                return Err(bad(
                    "Creating the Guest role requires admin.roles.create. No guest configuration was saved.",
                ));
            }
        }
    }
    // Reuse the same permission ceiling and role validation used at startup,
    // catching missing or unsafe roles before saving a configuration that fails.
    resolve_ephemeral_roles(&pool, "guest", &config.auth.guest.role_names)
        .await
        .map_err(|error| bad(error.detail()))?;
    let may_join: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM roles r JOIN role_permissions p ON p.role_id = r.id WHERE r.name = ANY($1) AND p.permission_name = 'world.instance.read')",
    )
    .bind(&config.auth.guest.role_names)
    .fetch_one(&pool).await.map_err(|_| bad("Could not verify guest participation permissions."))?;
    if !may_join {
        return Err(bad(
            "The configured guest roles need world.instance.read to join an Instance.",
        ));
    }
    pool.close().await;
    Ok(())
}

#[cfg(test)]
mod guest_settings_tests {
    use super::*;

    #[test]
    fn guest_toggle_preserves_other_methods_and_guest_settings() {
        let mut config = Config::default();
        config.auth.methods = vec!["local".into(), "name_only".into()];
        config.auth.guest.session_ttl_seconds = 7200;
        config.auth.guest.display_name_prefix = "Visitor".into();
        let mut table: toml::Table = "[auth.guest]\nsession_ttl_seconds=7200\ndisplay_name_prefix='Visitor'\n[server]\nbind='127.0.0.1:8080'".parse().unwrap();
        let id = orbisync_domain::WorldId::generate().to_string();
        edit_guest_config(
            &mut config,
            &mut table,
            &GuestSettingsEdit {
                enabled: true,
                allowed_worlds: vec![id.clone(), id.clone()],
            },
        )
        .unwrap();
        assert_eq!(config.auth.methods, ["local", "name_only", "guest"]);
        assert_eq!(config.auth.guest.role_names, ["Guest"]);
        assert_eq!(config.auth.guest.allowed_worlds, std::slice::from_ref(&id));
        assert_eq!(
            table["auth"]["guest"]["session_ttl_seconds"].as_integer(),
            Some(7200)
        );
        assert_eq!(
            table["auth"]["guest"]["display_name_prefix"].as_str(),
            Some("Visitor")
        );
        assert_eq!(table["server"]["bind"].as_str(), Some("127.0.0.1:8080"));
        edit_guest_config(
            &mut config,
            &mut table,
            &GuestSettingsEdit {
                enabled: false,
                allowed_worlds: vec![id.clone()],
            },
        )
        .unwrap();
        assert_eq!(config.auth.methods, ["local", "name_only"]);
        assert_eq!(config.auth.guest.allowed_worlds, [id]);
    }

    #[test]
    fn guest_toggle_rejects_malformed_and_non_v7_ids() {
        for id in ["not-a-world", "550e8400-e29b-41d4-a716-446655440000"] {
            assert!(
                edit_guest_config(
                    &mut Config::default(),
                    &mut toml::Table::new(),
                    &GuestSettingsEdit {
                        enabled: true,
                        allowed_worlds: vec![id.to_owned()]
                    }
                )
                .is_err()
            );
        }
        let mut config = Config::default();
        edit_guest_config(
            &mut config,
            &mut toml::Table::new(),
            &GuestSettingsEdit {
                enabled: true,
                allowed_worlds: vec![],
            },
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
