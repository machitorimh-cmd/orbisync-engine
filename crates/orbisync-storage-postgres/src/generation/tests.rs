#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use sqlx::postgres::PgConnectOptions;
use std::{path::PathBuf, time::Duration};

fn ordered(v: &serde_json::Value, fields: &[&str]) -> String {
    let fields: Vec<_> = fields
        .iter()
        .map(|key| {
            let value = match *key {
                "result" => ordered(
                    &v[key],
                    if v[key]["type"] == "applied" {
                        &["type", "response_payload"]
                    } else {
                        &["type", "code", "detail"]
                    },
                ),
                _ => v[key].to_string(),
            };
            format!("\"{key}\":{value}")
        })
        .collect();
    format!("{{{}}}", fields.join(","))
}
fn entity_json(v: &serde_json::Value) -> String {
    ordered(
        v,
        &[
            "id",
            "instance_id",
            "kind",
            "owner",
            "transform",
            "visibility",
            "revision",
            "created_at",
            "updated_at",
            "components",
        ],
    )
}
fn receipt_json(v: &serde_json::Value) -> String {
    ordered(
        v,
        &[
            "command_id",
            "fingerprint",
            "created_at_millis",
            "expires_at_millis",
            "message_id",
            "result",
        ],
    )
}

struct Source {
    m: StreamManifest,
    limits: CheckpointLimits,
    data: Vec<u8>,
    offset: usize,
    fail: Option<usize>,
    kill: Option<(PgPool, usize)>,
}
#[async_trait::async_trait]
impl GenerationSource for Source {
    fn limits(&self) -> CheckpointLimits {
        self.limits
    }
    fn manifest(&self) -> &StreamManifest {
        &self.m
    }
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ApplicationError> {
        if let Some((admin, page)) = &self.kill
            && *page == self.offset.div_ceil(self.limits.chunk_bytes())
        {
            sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND state='idle in transaction' AND query LIKE 'INSERT INTO checkpoint_chunks%'")
                    .execute(admin).await.map_err(db)?;
            self.kill = None;
        }
        if self.fail == Some(self.offset / self.limits.chunk_bytes()) {
            return Err(invalid());
        }
        if self.offset == self.data.len() {
            return Ok(None);
        }
        let end = (self.offset + self.limits.chunk_bytes()).min(self.data.len());
        let out = self.data[self.offset..end].to_vec();
        self.offset = end;
        Ok(Some(out))
    }
    async fn cancel(&mut self) {}
}
fn source(instance: InstanceId, millis: i64, command: Uuid) -> Source {
    let timestamp = time::OffsetDateTime::from_unix_timestamp_nanos(millis as i128 * 1_000_000)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let receipt = serde_json::json!({"command_id":command.to_string(),"fingerprint":vec![1u8;32],
        "message_id":command.to_string(),"created_at_millis":millis,"expires_at_millis":millis+86_400_000,
        "result":{"type":"applied","response_payload":vec![7u8;20000]}});
    let entity = serde_json::json!({"id":command.to_string(),"instance_id":instance.to_string(),"kind":"object","owner":null,
        "transform":null,"visibility":{"type":"global"},"revision":1,"created_at":timestamp,"updated_at":timestamp,"components":{"test.blob":vec![3u8;100]}});
    let entity = entity_json(&entity);
    let receipt = receipt_json(&receipt);
    let data=format!("{{\"format_version\":5,\"instance_id\":\"{instance}\",\"revision\":1,\"timestamp\":\"{timestamp}\",\"entities\":[{entity}],\"dedup\":[{receipt}]}}").into_bytes();
    let limits = CheckpointLimits::new(16384, 2097152).unwrap();
    let m = StreamManifest {
        serialized_bytes: data.len() as u64,
        chunk_bytes: 16384,
        chunk_count: data.len().div_ceil(16384) as u64,
        digest: Sha256::digest(&data).into(),
    };
    Source {
        m,
        limits,
        data,
        offset: 0,
        fail: None,
        kill: None,
    }
}
fn attempt(
    s: &Source,
    i: InstanceId,
    writer: &PgWriterOwnership,
    head: i64,
    time: i64,
) -> GenerationAttempt {
    GenerationAttempt::from_manifest(i, head, writer.permit().token(), time, 5, s.limits, &s.m)
        .unwrap()
}
fn retime(source: &mut Source, millis: i64, ttl: i64) {
    let mut v: serde_json::Value = serde_json::from_slice(&source.data).unwrap();
    let timestamp = time::OffsetDateTime::from_unix_timestamp_nanos(millis as i128 * 1_000_000)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    for r in v["dedup"].as_array_mut().unwrap() {
        r["created_at_millis"] = millis.into();
        r["expires_at_millis"] = (millis + ttl).into();
    }
    let entities = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(entity_json)
        .collect::<Vec<_>>()
        .join(",");
    let receipts = v["dedup"]
        .as_array()
        .unwrap()
        .iter()
        .map(receipt_json)
        .collect::<Vec<_>>()
        .join(",");
    source.data=format!("{{\"format_version\":5,\"instance_id\":{},\"revision\":{},\"timestamp\":\"{timestamp}\",\"entities\":[{entities}],\"dedup\":[{receipts}]}}",v["instance_id"],v["revision"]).into_bytes();
    source.m.serialized_bytes = source.data.len() as u64;
    source.m.chunk_count = source.data.len().div_ceil(16384) as u64;
    source.m.digest = Sha256::digest(&source.data).into();
    source.offset = 0;
}

#[test]
fn independent_record_validation_rejects_corruption() {
    let instance = InstanceId::new(Uuid::now_v7()).unwrap();
    let command = Uuid::now_v7();
    let s = source(instance, 1000, command);
    let a = GenerationAttempt::from_manifest(
        instance,
        0,
        orbisync_application::checkpoint_admission::WriterToken {
            epoch: 1,
            boot: Uuid::now_v7(),
        },
        1000,
        5,
        s.limits,
        &s.m,
    )
    .unwrap();
    let check = |data: &[u8]| {
        let mut validator = validate::Validator::new(&a);
        for chunk in data.chunks(137) {
            validator.feed(chunk)?;
        }
        validator.finish()
    };
    assert!(check(&s.data).is_ok());
    let original = String::from_utf8(s.data.clone()).unwrap();
    for (from, to) in [
        (
            "\"kind\":\"object\"",
            "\"kind\":\"object\",\"kind\":\"avatar\"",
        ),
        ("test.blob", "core.blob"),
        ("\"type\":\"global\"", "\"type\":\"bad\""),
        ("\"revision\":1", "\"revision\":0"),
        ("\"created_at_millis\":1000", "\"created_at_millis\":1001"),
    ] {
        assert!(
            check(original.replace(from, to).as_bytes()).is_err(),
            "accepted {from}"
        );
    }
    assert!(check(&s.data[..s.data.len() - 1]).is_err());
    let value: serde_json::Value = serde_json::from_slice(&s.data).unwrap();
    let entity_digest = Sha256::digest(entity_json(&value["entities"][0]).as_bytes());
    let receipt_digest = Sha256::digest(receipt_json(&value["dedup"][0]).as_bytes());
    for window in [1, 2, 7, 137, 16384] {
        let mut validator = validate::Validator::new(&a);
        for part in s.data.chunks(window) {
            validator.feed(part).unwrap();
        }
        validator.finish().unwrap();
        assert_eq!(validator.state.finalize(), Sha256::digest(entity_digest));
        let receipt = &validator
            .receipts
            .iter()
            .find(|(id, _)| *id == command)
            .unwrap()
            .1;
        assert_eq!(receipt.digest, receipt_digest.as_slice());
        assert_eq!((receipt.created, receipt.expires), (1000, 86_401_000));
    }
    let mut old = a.clone();
    old.codec = 4;
    let mut validator = validate::Validator::new(&old);
    assert!(
        validator
            .feed(&s.data)
            .unwrap_err()
            .to_string()
            .contains("reconcile/export")
    );
    assert!(
        check(
            original
                .replace("\"format_version\":5", "\"format_version\":4")
                .as_bytes()
        )
        .is_err()
    );
    assert!(
        check(
            original
                .replace("\"type\":\"global\"", "\"type\":\"global\",\"opaque\":0")
                .as_bytes()
        )
        .is_err()
    );
}

#[test]
fn shared_codec5_golden_storage_digest() {
    let entity = include_str!("../../../../test-vectors/checkpoint-codec5-entity.json");
    let instance = InstanceId::parse("01900000-0000-7000-8000-000000000002").unwrap();
    let data = format!("{{\"format_version\":5,\"instance_id\":\"{instance}\",\"revision\":1,\"timestamp\":\"2024-01-01T00:00:00Z\",\"entities\":[{entity}],\"dedup\":[]}}").into_bytes();
    let manifest = StreamManifest {
        serialized_bytes: data.len() as u64,
        chunk_bytes: 16384,
        chunk_count: 1,
        digest: Sha256::digest(&data).into(),
    };
    let attempt = GenerationAttempt::from_manifest(
        instance,
        0,
        orbisync_application::checkpoint_admission::WriterToken {
            epoch: 1,
            boot: Uuid::now_v7(),
        },
        1_704_067_200_000,
        5,
        CheckpointLimits::default(),
        &manifest,
    )
    .unwrap();
    let mut validator = validate::Validator::new(&attempt);
    for part in data.chunks(1) {
        validator.feed(part).unwrap();
    }
    validator.finish().unwrap();
    assert_eq!(
        validator.state.finalize(),
        Sha256::digest(Sha256::digest(entity.as_bytes()))
    );
}
async fn collect(mut source: PgGenerationSource) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = source.next_chunk().await.unwrap() {
        bytes.extend(chunk);
    }
    bytes
}

/// Requires an explicitly supplied disposable database. Never silently skips.
#[tokio::test]
#[ignore = "requires isolated ORBISYNC_PHASE3_DATABASE_URL"]
async fn real_pg_generation_contract() {
    let url = std::env::var("ORBISYNC_PHASE3_DATABASE_URL").expect("isolated PG URL required");
    let bootstrap = PgPool::connect(&url).await.unwrap();
    let database = format!("phase3_{}", Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&bootstrap)
        .await
        .unwrap();
    let root_options: PgConnectOptions = url.parse().unwrap();
    let root_options = root_options.database(&database);
    let admin = PgPool::connect_with(root_options.clone()).await.unwrap();
    eprintln!("isolated database: {database}");
    crate::MIGRATOR.run(&admin).await.unwrap();
    let role = format!("phase3_{}", Uuid::now_v7().simple());
    let password = Uuid::now_v7().simple().to_string();
    sqlx::raw_sql(&format!("CREATE ROLE {role} LOGIN PASSWORD '{password}'; GRANT USAGE ON SCHEMA public TO {role}; GRANT SELECT ON ALL TABLES IN SCHEMA public TO {role}; GRANT INSERT,UPDATE,DELETE ON checkpoint_generations,checkpoint_chunks,checkpoint_heads,checkpoint_retained,checkpoint_projection,checkpoint_projection_pins,checkpoint_retiring,checkpoint_generation_receipts TO {role}; GRANT EXECUTE ON FUNCTION checkpoint_start_writer(TEXT,UUID),checkpoint_start_writer(TEXT,UUID,INTEGER),checkpoint_begin_conversion(UUID,UUID,BYTEA,TEXT),checkpoint_lock_writer(BIGINT,UUID),checkpoint_is_referenced(UUID,UUID) TO {role};"))
        .execute(&admin).await.unwrap();
    let legacy = format!("legacy_{}", Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE ROLE {legacy} NOLOGIN"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("UPDATE checkpoint_writer_control SET protocol=5,deployment='isolated-test',writer_role=$1::name,old_writer_roles=ARRAY[$2::name],exclusion_report='isolated fixture stopped old writer' WHERE singleton").bind(&role).bind(&legacy).execute(&admin).await.unwrap();
    let opts = root_options;
    let opts = opts
        .username(&role)
        .password(&password)
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    use sqlx::ConnectOptions;
    let runtime_url = opts.to_url_lossy().to_string();
    let lock: PathBuf = std::env::temp_dir().join(format!("{role}.lock"));
    sqlx::query(&format!("ALTER ROLE {legacy} LOGIN"))
        .execute(&admin)
        .await
        .unwrap();
    assert!(
        PgWriterOwnership::acquire(&runtime_url, &lock, "isolated-test")
            .await
            .is_err()
    );
    sqlx::query(&format!("ALTER ROLE {legacy} NOLOGIN"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("GRANT INSERT ON instance_checkpoints TO {role}"))
        .execute(&admin)
        .await
        .unwrap();
    assert!(
        PgWriterOwnership::acquire(&runtime_url, &lock, "isolated-test")
            .await
            .is_err()
    );
    sqlx::query(&format!(
        "REVOKE INSERT ON instance_checkpoints FROM {role}"
    ))
    .execute(&admin)
    .await
    .unwrap();
    let owner = PgWriterOwnership::acquire(&runtime_url, &lock, "isolated-test")
        .await
        .unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut candidate = std::process::Command::new(&executable)
        .args([
            "--ignored",
            "--exact",
            "generation::tests::ownership_candidate",
        ])
        .env("ORBISYNC_PHASE3_CANDIDATE_URL", &runtime_url)
        .env("ORBISYNC_PHASE3_CANDIDATE_LOCK", &lock)
        .spawn()
        .unwrap();
    eprintln!(
        "ownership candidate PID={} start={} path={}",
        candidate.id(),
        time::OffsetDateTime::now_utc(),
        executable.display()
    );
    assert!(candidate.wait().unwrap().success());
    assert!(
        PgWriterOwnership::acquire(&runtime_url, &lock, "isolated-test")
            .await
            .is_err()
    );
    let other_lock = lock.with_extension("second");
    assert!(
        PgWriterOwnership::acquire(&runtime_url, &other_lock, "isolated-test")
            .await
            .is_err()
    );
    let pool = PgPool::connect_with(opts.clone()).await.unwrap();
    assert!(
        sqlx::query("DELETE FROM instance_checkpoints")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM persistent_entities")
            .execute(&pool)
            .await
            .is_err()
    );
    let world = Uuid::now_v7();
    let instance = InstanceId::new(Uuid::now_v7()).unwrap();
    sqlx::query(
        "INSERT INTO world_definitions VALUES($1,'phase3',NULL,'active',1,'{}','{}',1,now(),now())",
    )
    .bind(world)
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query("INSERT INTO world_instances VALUES($1,$2,'created',1,now(),NULL,1)")
        .bind(instance.as_uuid())
        .bind(world)
        .execute(&admin)
        .await
        .unwrap();
    // Explicit isolated fixture baseline; production classification is phase 4.
    sqlx::query("INSERT INTO checkpoint_authority(instance_id,status,report) VALUES($1,'reconciled_ready','isolated proven-empty fixture')")
        .bind(instance.as_uuid()).execute(&admin).await.unwrap();
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let command = Uuid::now_v7();
    let mut s = source(instance, now, command);
    let a = attempt(&s, instance, &owner, 0, now);
    let store = PgGenerationStore::new(pool.clone(), s.limits, &owner);
    let approval = ReconciledAuthority {
        source_id: None,
        source_digest: None,
        report: "isolated proven-empty fixture".into(),
    };
    assert_eq!(
        store.resolve(&a).await.unwrap(),
        GenerationResolution::ProvenAbsent
    );
    assert!(store.publish(&a, &mut s).await.is_err());
    let wrong_approval = ReconciledAuthority {
        report: "unreviewed".into(),
        ..approval.clone()
    };
    assert!(
        store
            .publish_reconciled(&a, &mut s, &wrong_approval)
            .await
            .is_err()
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM checkpoint_authority WHERE instance_id=$1")
            .bind(instance.as_uuid())
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(status, "reconciled_ready");
    for failure in 0..=2 {
        s.offset = 0;
        s.fail = Some(failure);
        assert!(
            store
                .publish_reconciled(&a, &mut s, &approval)
                .await
                .is_err()
        );
        assert_eq!(
            store.resolve(&a).await.unwrap(),
            GenerationResolution::ProvenAbsent
        );
    }
    for page in 1..=3 {
        s.offset = 0;
        s.fail = None;
        s.kill = Some((admin.clone(), page));
        assert!(
            store
                .publish_reconciled(&a, &mut s, &approval)
                .await
                .is_err()
        );
        assert_eq!(
            store.resolve(&a).await.unwrap(),
            GenerationResolution::ProvenAbsent
        );
        assert!(store.cleanup(instance).await.is_err());
        let mut changed = a.clone();
        changed.digest[0] ^= 1;
        assert_eq!(
            store.resolve(&changed).await.unwrap(),
            GenerationResolution::Fenced
        );
    }
    s.offset = 0;
    s.fail = None;
    assert_eq!(
        store
            .publish_reconciled(&a, &mut s, &approval)
            .await
            .unwrap(),
        GenerationResolution::Committed { publish_seq: 1 }
    );
    // Simulated lost caller acknowledgment: resolve, then retry exact ID/bytes.
    assert_eq!(
        store.resolve(&a).await.unwrap(),
        GenerationResolution::Committed { publish_seq: 1 }
    );
    s.offset = 0;
    assert_eq!(
        store
            .publish_reconciled(&a, &mut s, &approval)
            .await
            .unwrap(),
        GenerationResolution::Committed { publish_seq: 1 }
    );
    assert_eq!(collect(store.load(instance).await.unwrap()).await, s.data);
    let mut wrong = a.clone();
    wrong.digest[0] ^= 1;
    assert_eq!(
        store.resolve(&wrong).await.unwrap(),
        GenerationResolution::Fenced
    );
    let mut stale = a.clone();
    stale.generation_id = Uuid::now_v7();
    assert_eq!(
        store.resolve(&stale).await.unwrap(),
        GenerationResolution::Fenced
    );
    // Real PostgreSQL COMMIT completes, but a loopback wire proxy discards its
    // CommandComplete response and closes the client connection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    eprintln!("owned commit-ack proxy loopback port={proxy_port}");
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = armed.clone();
    let upstream = opts.get_port();
    let proxy = tokio::spawn(async move {
        loop {
            let (client, _) = listener.accept().await.unwrap();
            let flag = flag.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let server = tokio::net::TcpStream::connect(("127.0.0.1", upstream))
                    .await
                    .unwrap();
                let (mut cr, mut cw) = client.into_split();
                let (mut sr, mut sw) = server.into_split();
                let forward = tokio::io::copy(&mut cr, &mut sw);
                let reverse = async {
                    loop {
                        let mut header = [0u8; 5];
                        if sr.read_exact(&mut header).await.is_err() {
                            break;
                        }
                        let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
                        assert!((4..=2_097_152).contains(&length));
                        let mut body = vec![0; length - 4];
                        if sr.read_exact(&mut body).await.is_err() {
                            break;
                        }
                        if header[0] == b'C'
                            && body == b"COMMIT\0"
                            && flag.swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            break;
                        }
                        if cw.write_all(&header).await.is_err()
                            || cw.write_all(&body).await.is_err()
                        {
                            break;
                        }
                    }
                };
                tokio::select! {_result=forward=>{},()=reverse=>{}}
            });
        }
    });
    let proxy_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(opts.clone().port(proxy_port))
        .await
        .unwrap();
    let uncertain_store = PgGenerationStore::new(proxy_pool.clone(), s.limits, &owner);
    let lost = attempt(&s, instance, &owner, 1, now);
    s.offset = 0;
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        uncertain_store.publish(&lost, &mut s).await.unwrap(),
        GenerationResolution::Uncertain
    );
    assert!(store.cleanup(instance).await.is_err());
    let fresh = attempt(&s, instance, &owner, 2, now);
    s.offset = 0;
    assert_eq!(
        store.publish(&fresh, &mut s).await.unwrap(),
        GenerationResolution::Fenced
    );
    assert_eq!(
        store.resolve(&lost).await.unwrap(),
        GenerationResolution::Committed { publish_seq: 2 }
    );
    proxy_pool.close().await;
    proxy.abort();
    let _joined = proxy.await;
    let mut pin = store.locked(instance).await.unwrap();
    sqlx::query("INSERT INTO checkpoint_projection_pins VALUES($1,$2)")
        .bind(instance.as_uuid())
        .bind(a.generation_id)
        .execute(&mut *pin)
        .await
        .unwrap();
    pin.commit().await.unwrap();
    // Hold a real RR reader across head changes and collection.
    let reader = store.load(instance).await.unwrap();
    for head in 2..5 {
        s.offset = 0;
        let next = attempt(&s, instance, &owner, head, now);
        assert_eq!(
            store.publish(&next, &mut s).await.unwrap(),
            GenerationResolution::Committed {
                publish_seq: head + 1
            }
        );
    }
    assert!(store.cleanup(instance).await.unwrap() > 0);
    assert_eq!(
        store.resolve(&a).await.unwrap(),
        GenerationResolution::Committed { publish_seq: 1 }
    );
    let mut pin = store.locked(instance).await.unwrap();
    sqlx::query("DELETE FROM checkpoint_projection_pins WHERE instance_id=$1")
        .bind(instance.as_uuid())
        .execute(&mut *pin)
        .await
        .unwrap();
    pin.commit().await.unwrap();
    assert!(store.cleanup(instance).await.unwrap() > 0);
    assert_eq!(collect(reader).await, s.data);
    assert_eq!(collect(store.load(instance).await.unwrap()).await, s.data);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM checkpoint_retained WHERE instance_id=$1")
            .bind(instance.as_uuid())
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(n, 3);
    // Original ID cannot be reinstalled after collection/newer head.
    assert_eq!(
        store.resolve(&a).await.unwrap(),
        GenerationResolution::Fenced
    );
    // Missing final chunk is rejected by a deferred SQL constraint, independent of Rust.
    let mut tx = store.locked(instance).await.unwrap();
    let bad = Uuid::now_v7();
    sqlx::query("INSERT INTO checkpoint_generations SELECT instance_id,$2,6,revision,epoch,boot,codec,completed_millis,total,chunk_bytes,chunk_count,digest,state_digest FROM checkpoint_generations WHERE instance_id=$1 LIMIT 1")
        .bind(instance.as_uuid()).bind(bad).execute(&mut *tx).await.unwrap();
    assert!(tx.commit().await.is_err());
    // Same-instance and exact head sequence are enforced by SQL, not only Rust.
    let mut tx = store.locked(instance).await.unwrap();
    assert!(
        sqlx::query("UPDATE checkpoint_heads SET publish_seq=999 WHERE instance_id=$1")
            .bind(instance.as_uuid())
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let second = InstanceId::new(Uuid::now_v7()).unwrap();
    sqlx::query("INSERT INTO world_instances VALUES($1,$2,'created',1,now(),NULL,1)")
        .bind(second.as_uuid())
        .bind(world)
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("INSERT INTO checkpoint_authority(instance_id,status,report) VALUES($1,'reconciled_ready','isolated empty second instance')").bind(second.as_uuid()).execute(&admin).await.unwrap();
    // Expired uncommitted G is resolved absent before G2 gets fresh receipt time.
    let second_approval = ReconciledAuthority {
        source_id: None,
        source_digest: None,
        report: "isolated empty second instance".into(),
    };
    let aged_command = Uuid::now_v7();
    let mut aged = source(second, now - 86_400_000 - 120000, aged_command);
    let aged_attempt = attempt(&aged, second, &owner, 0, now - 86_400_000 - 120000);
    assert!(
        store
            .publish_reconciled(&aged_attempt, &mut aged, &second_approval)
            .await
            .is_err()
    );
    assert_eq!(
        store.resolve(&aged_attempt).await.unwrap(),
        GenerationResolution::ProvenAbsent
    );
    store.retire_absent(&aged_attempt).await.unwrap();
    let fresh_time = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
        - 86_400_000
        + 1500;
    retime(&mut aged, fresh_time, 86_400_000);
    let fresh_attempt = attempt(&aged, second, &owner, 0, fresh_time);
    assert_ne!(fresh_attempt.generation_id, aged_attempt.generation_id);
    assert_eq!(
        store
            .publish_reconciled(&fresh_attempt, &mut aged, &second_approval)
            .await
            .unwrap(),
        GenerationResolution::Committed { publish_seq: 1 }
    );
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(
        store.resolve(&fresh_attempt).await.unwrap(),
        GenerationResolution::Committed { publish_seq: 1 }
    );
    // Stored commitment survives expiry; a later generation cannot renew it.
    let renewed_time = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    retime(&mut aged, renewed_time, 86_400_000);
    let renew = attempt(&aged, second, &owner, 1, renewed_time);
    assert!(store.publish(&renew, &mut aged).await.is_err());
    assert_eq!(
        store.resolve(&renew).await.unwrap(),
        GenerationResolution::ProvenAbsent
    );
    store.retire_absent(&renew).await.unwrap();
    let current: Uuid =
        sqlx::query_scalar("SELECT generation_id FROM checkpoint_heads WHERE instance_id=$1")
            .bind(instance.as_uuid())
            .fetch_one(&admin)
            .await
            .unwrap();
    let mut tx = store.locked(second).await.unwrap();
    assert!(
        sqlx::query("INSERT INTO checkpoint_projection_pins VALUES($1,$2)")
            .bind(second.as_uuid())
            .bind(current)
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut tx = store.locked(instance).await.unwrap();
    assert!(sqlx::query("DELETE FROM checkpoint_chunks WHERE instance_id=$1 AND generation_id=$2 AND chunk_index=0").bind(instance.as_uuid()).bind(current).execute(&mut *tx).await.is_err());
    tx.rollback().await.unwrap();
    // Equal revision may append immutable outcomes; renewing an old receipt is rejected.
    let mut renewal = source(instance, now, command);
    retime(&mut renewal, now + 1000, 86_400_000);
    let renew_attempt = attempt(&renewal, instance, &owner, 5, now + 1000);
    assert!(store.publish(&renew_attempt, &mut renewal).await.is_err());
    assert_eq!(
        store.resolve(&renew_attempt).await.unwrap(),
        GenerationResolution::ProvenAbsent
    );
    store.retire_absent(&renew_attempt).await.unwrap();
    let mut more: serde_json::Value = serde_json::from_slice(&s.data).unwrap();
    let mut receipt = more["dedup"][0].take();
    let new_command = Uuid::now_v7().to_string();
    receipt["command_id"] = new_command.clone().into();
    receipt["message_id"] = new_command.into();
    s.data.truncate(s.data.len() - 2);
    s.data.push(b',');
    s.data.extend(receipt_json(&receipt).as_bytes());
    s.data.extend(b"]}");
    s.m.serialized_bytes = s.data.len() as u64;
    s.m.chunk_count = s.data.len().div_ceil(16384) as u64;
    s.m.digest = Sha256::digest(&s.data).into();
    s.offset = 0;
    let equal = attempt(&s, instance, &owner, 5, now);
    assert_eq!(
        store.publish(&equal, &mut s).await.unwrap(),
        GenerationResolution::Committed { publish_seq: 6 }
    );
    sqlx::raw_sql("CREATE FUNCTION phase3_head_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated publication fault'; END $$")
        .execute(&admin).await.unwrap();
    for timing in [
        "BEFORE INSERT OR UPDATE",
        "AFTER INSERT OR UPDATE",
        "DEFERRED",
    ] {
        let sql = if timing == "DEFERRED" {
            "CREATE CONSTRAINT TRIGGER phase3_fault AFTER INSERT OR UPDATE ON checkpoint_heads DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION phase3_head_fault()".to_string()
        } else {
            format!(
                "CREATE TRIGGER phase3_fault {timing} ON checkpoint_heads FOR EACH ROW EXECUTE FUNCTION phase3_head_fault()"
            )
        };
        sqlx::query(&sql).execute(&admin).await.unwrap();
        let failed = attempt(&s, instance, &owner, 6, now);
        s.offset = 0;
        let result = store.publish(&failed, &mut s).await;
        if timing == "DEFERRED" {
            assert_eq!(result.unwrap(), GenerationResolution::Uncertain);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(
            store.resolve(&failed).await.unwrap(),
            GenerationResolution::ProvenAbsent
        );
        store.retire_absent(&failed).await.unwrap();
        let selected = store.load(instance).await.unwrap();
        assert_eq!(selected.selected().expected_head + 1, 6);
        assert_eq!(collect(selected).await, s.data);
        sqlx::query("DROP TRIGGER phase3_fault ON checkpoint_heads")
            .execute(&admin)
            .await
            .unwrap();
    }
    // Dedicated connection loss permanently invalidates actor capabilities.
    let permit = owner.permit();
    sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype='advisory' AND classid=19419 AND objid=4 AND objsubid=2")
        .execute(&admin).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!permit.is_live());
    assert!(store.load(instance).await.is_err());
    owner.shutdown().await;
    let restarted = PgWriterOwnership::acquire(&runtime_url, &lock, "isolated-test")
        .await
        .unwrap();
    assert!(restarted.permit().token().epoch > a.writer.epoch);
    let replacement = PgGenerationStore::new(pool.clone(), s.limits, &restarted);
    assert_eq!(
        replacement.resolve(&a).await.unwrap(),
        GenerationResolution::Fenced
    );
    assert_eq!(
        collect(replacement.load(instance).await.unwrap()).await,
        s.data
    );
    if let Ok(container) = std::env::var("ORBISYNC_PHASE3_RESTART_CONTAINER") {
        let inspected=std::process::Command::new("docker").args(["inspect","--format","{{.Id}}|{{index .Config.Labels \"ao.session\"}}|{{.State.Pid}}|{{.State.StartedAt}}|{{.Path}}",&container]).output().unwrap();
        assert!(inspected.status.success());
        let record = String::from_utf8(inspected.stdout).unwrap();
        let fields: Vec<_> = record.trim().split('|').collect();
        assert_eq!(fields[0], container);
        assert_eq!(fields[1], std::env::var("AO_SESSION_ID").unwrap());
        eprintln!("owned PostgreSQL restart: {}", record.trim());
        let result = std::process::Command::new("docker")
            .args(["restart", "--time", "3", &container])
            .output()
            .unwrap();
        assert!(result.status.success());
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!restarted.permit().is_live());
        assert!(replacement.load(instance).await.is_err());
    } else {
        eprintln!("PostgreSQL container restart not requested; not restart gate evidence");
    }
    restarted.shutdown().await;
    // Fresh process acquires a new boot and restores identical persisted bytes.
    let digest =
        s.m.digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
    let mut child = std::process::Command::new(&executable)
        .args(["--ignored", "--exact", "generation::tests::restart_reader"])
        .env("ORBISYNC_PHASE3_CANDIDATE_URL", &runtime_url)
        .env("ORBISYNC_PHASE3_CANDIDATE_LOCK", &lock)
        .env("ORBISYNC_PHASE3_INSTANCE", instance.to_string())
        .env("ORBISYNC_PHASE3_DIGEST", digest)
        .spawn()
        .unwrap();
    eprintln!(
        "restore process PID={} start={} path={}",
        child.id(),
        time::OffsetDateTime::now_utc(),
        executable.display()
    );
    assert!(child.wait().unwrap().success());
    pool.close().await;
    admin.close().await;
    let _removed = std::fs::remove_file(lock);
    let _removed = std::fs::remove_file(other_lock);
}

#[tokio::test]
#[ignore = "subprocess restore helper requires explicit isolated environment"]
async fn restart_reader() {
    let url = std::env::var("ORBISYNC_PHASE3_CANDIDATE_URL").expect("candidate URL");
    let lock =
        PathBuf::from(std::env::var("ORBISYNC_PHASE3_CANDIDATE_LOCK").expect("candidate lock"));
    let instance = InstanceId::parse(&std::env::var("ORBISYNC_PHASE3_INSTANCE").unwrap()).unwrap();
    let owner = PgWriterOwnership::acquire(&url, &lock, "isolated-test")
        .await
        .unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    let store = PgGenerationStore::new(
        pool.clone(),
        CheckpointLimits::new(16384, 2097152).unwrap(),
        &owner,
    );
    let data = collect(store.load(instance).await.unwrap()).await;
    let digest = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(digest, std::env::var("ORBISYNC_PHASE3_DIGEST").unwrap());
    owner.shutdown().await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "subprocess helper requires explicit isolated candidate environment"]
async fn ownership_candidate() {
    let url = std::env::var("ORBISYNC_PHASE3_CANDIDATE_URL").expect("candidate URL");
    let lock =
        PathBuf::from(std::env::var("ORBISYNC_PHASE3_CANDIDATE_LOCK").expect("candidate lock"));
    assert!(
        PgWriterOwnership::acquire(&url, &lock, "isolated-test")
            .await
            .is_err()
    );
    let separate = lock.with_extension("candidate");
    assert!(
        PgWriterOwnership::acquire(&url, &separate, "isolated-test")
            .await
            .is_err()
    );
    let _removed = std::fs::remove_file(separate);
}
