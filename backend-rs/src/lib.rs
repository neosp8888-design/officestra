// OFFICESTRA 백엔드 Rust 리뉴얼의 라이브러리 루트. 모듈은 Node의 backend/src 파일 구조를 따른다.

pub mod agent_event_parser;
pub mod agent_provider;
pub mod agent_runtime;
pub mod antigravity_local_state;
pub mod antigravity_model_selection;
pub mod antigravity_playwright;
pub mod antigravity_reasoning;
pub mod backfill_rag_embeddings;
pub mod benchmark_rag_search;
pub mod character_profile;
pub mod character_settings;
pub mod child_process;
pub mod claude_cost_state;
pub mod claude_persistent_worker;
pub mod cli_updates;
pub mod codex_context_compactor;
pub mod codex_rollout_collaboration;
pub mod codex_rollout_turns;
pub mod config;
pub mod configuration;
pub mod db;
pub mod developer_tool_events;
pub mod git_workspace;
pub mod health;
pub mod http;
pub mod js;
pub mod local_agent_profile;
pub mod local_artifacts;
pub mod local_embedding;
pub mod local_model_capabilities;
pub mod local_profile_selection;
pub mod local_provider;
pub mod migrate;
pub mod model_catalog;
pub mod pricing_catalog;
pub mod rag;
pub mod realtime;
pub mod reply_delivery;
pub mod roleplay;
pub mod runtime_cli_paths;
pub mod session_context_usage;
pub mod slack_bridge;
pub mod structured_turn_result;
pub mod telegram_bridge;
pub mod terminal_sessions;
pub mod terminal_turn_activities;
pub mod terminal_usage;
pub mod token_cost_estimator;
pub mod turn_deletion;
pub mod turn_feed;
pub mod turn_feedback;
pub mod usage_report;
pub mod usage_summary;
pub mod voice;
pub mod wiki_knowledge;
pub mod work_board;
pub mod work_record_memory;
pub mod work_record_provenance;

use std::path::Path;
use std::sync::Arc;

use config::Config;
use health::Runtime;
use http::AppState;

/// 밖으로 나가는 HTTPS 요청용 reqwest 빌더. 클라이언트를 만들기 전에 sqlx와 같은 ring 공급자를 한 번 설치한다.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
}

/// 설정을 읽고 마이그레이션·직원 동기화를 마친 뒤 127.0.0.1에서 HTTP 서버를 띄운다. Node server.mjs의 부팅 순서와 같다.
pub async fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url)
        .await
        .map_err(|error| format!("PostgreSQL에 연결할 수 없습니다. {error}"))?;
    migrate::run(&pool, &config.migrations_dir)
        .await
        .map_err(|error| error.to_string())?;

    let configuration = configuration::read_character_configuration(
        &config.character_config_path,
        config.workdir.as_deref().and_then(Path::to_str),
    )?;
    let Some(workdir) = configuration::configuration_workdir(&configuration) else {
        return Err("직원 설정에 workdir이 없습니다. OFFICE_WORKDIR을 지정하세요.".to_string());
    };
    let repository_root = configuration::canonical_project_root(&workdir).await?;
    let workspace_manager = git_workspace::GitWorkspaceManager::new(
        &workdir.to_string_lossy(),
        config.worktree_root.as_deref().map(|root| root.to_string_lossy()).as_deref(),
    )
    .map_err(|error| error.message)?;
    let mut transaction = pool.begin().await.map_err(|error| error.to_string())?;
    configuration::sync_characters(&mut transaction, &configuration)
        .await
        .map_err(|error| format!("직원 설정을 동기화하지 못했습니다. {error}"))?;
    transaction.commit().await.map_err(|error| error.to_string())?;
    let pricing = pricing_catalog::PricingCatalogService::new(
        pricing_catalog::PostgresPricingCatalogStore::new(pool.clone()),
        pricing_catalog::HttpPricingFetcher::new()?,
        pricing_catalog::PricingServiceOptions::default(),
    );
    if let Err(error) = pricing.load_cached().await {
        eprintln!("저장된 가격 카탈로그를 읽지 못해 내장 기본값으로 시작합니다. {error}");
    }
    pricing.start();
    let cost_summary = Arc::new(usage_report::TurnCostSummaryCache::default());
    let realtime = Arc::new(realtime::Realtime::new(Arc::clone(&cost_summary)));
    let catalog_realtime = Arc::clone(&realtime);
    let model_catalog = model_catalog::ModelCatalogService::new(
        Arc::new(model_catalog::PostgresModelCatalogStore::new(pool.clone())),
        Arc::new(model_catalog::SystemModelCatalogHost::new(pool.clone())),
        model_catalog::ModelCatalogOptions {
            on_changed: Arc::new(move |provider| {
                catalog_realtime.broadcast(&serde_json::json!({ "type": "model-catalog.changed", "provider": provider }));
            }),
            ..Default::default()
        },
    );
    if let Err(error) = model_catalog.load_cached().await {
        eprintln!("저장된 모델 카탈로그를 읽지 못해 내장 기본값으로 시작합니다. {error}");
    }
    model_catalog.start();
    let root = repository_root.to_str().map(str::to_string);
    let reconciled = db::with_transaction(&pool, async |connection| {
        work_record_memory::reconcile_terminal_work_record_reviews(connection, root.as_deref()).await
    })
    .await;
    if let Err(error) = reconciled {
        eprintln!("종료된 작업 기록 상태를 재조정하지 못했지만 기동을 계속합니다. {}", rag::database_message(&error));
    }
    let synced = db::with_transaction(&pool, async |connection| {
        work_record_memory::sync_work_record_rag_documents(connection, root.as_deref(), None).await
    })
    .await;
    if let Err(error) = synced {
        eprintln!("파생 RAG 재색인에 실패했지만 백엔드를 계속 시작합니다. {}", rag::database_message(&error));
    }

    let embedding = local_embedding::LocalEmbeddingService::default();
    let path_text = |path: &Path| path.to_string_lossy().into_owned();
    let result_tool_directory = config.result_tool_dir.as_deref().map(path_text).unwrap_or_else(|| {
        std::env::current_exe().ok().and_then(|p| p.parent().map(path_text))
            .unwrap_or_else(|| path_text(&repository_root))
    });
    let playwright_driver_directory = config
        .playwright_driver_dir
        .as_deref()
        .map(path_text)
        .unwrap_or_else(|| js::join_path(&[&result_tool_directory, "playwright-driver-1.57.0"]));
    let paths = agent_runtime::launch::LaunchPaths {
        result_tool_directory,
        node_executable: config.node_executable.as_deref().map(path_text),
        playwright_driver_directory,
        runtime_id: std::process::id().to_string(),
    };
    let usage_summary = usage_summary::UsageSummaryReader::new(
        usage_summary::UsageProviders::system(usage_summary::UsageSources::from_config(
            pool.clone(),
            &config,
            Some(paths.playwright_driver_directory.clone()),
            paths.node_executable.clone(),
        )),
        usage_summary::UsageSummaryOptions::default(),
    );
    let runtime = Arc::new(Runtime::new(Some(workdir), Some(repository_root.clone())));
    let runtime_realtime = Arc::clone(&realtime);
    let roots = agent_runtime::service::SessionRoots { home: local_artifacts::home_directory() };
    let mut services = agent_runtime::service::RuntimeServices::new(
        pool.clone(),
        Arc::new(move |event| runtime_realtime.broadcast(&event)),
        paths.clone(),
        roots.clone(),
    );
    services.embedding = Some(embedding.clone());
    services.workspace_manager = Some(Arc::new(workspace_manager));
    // Node처럼 저장소 아래 .office-local-resources에 4090 소유 기록을 둔다.
    let local_realtime = Arc::clone(&realtime);
    let local_providers = local_provider::service::LocalProviderService::new(local_provider::service::ServiceOptions::new(
        pool.clone(),
        repository_root.join(".office-local-resources"),
        Arc::new(move |event| local_realtime.broadcast(&event)),
    ))?;
    services.local_providers = Some(local_providers.clone());
    runtime.attach_services(services);
    // Node처럼 실행기 다음, 중단 턴 정리 전에 터미널 관리자를 붙인다.
    let terminal_options = terminal_sessions::TerminalManagerOptions {
        port: config.port,
        ..terminal_sessions::TerminalManagerOptions::new(
            roots,
            paths,
            local_embedding::process_environment(),
            terminal_sessions::default_hook_path(),
        )
    };
    let terminal_sessions = Arc::new(terminal_sessions::TerminalSessionManager::new(Arc::clone(&runtime), terminal_options));
    runtime.attach_terminal_sessions(&terminal_sessions);
    // Node처럼 듣기 전에 재시작 전 실행 중이던 턴을 정리한다. 실패하면 시작하지 않는다.
    runtime.recover_interrupted_jobs().await.map_err(|error| error.to_string())?;
    let delivery_realtime = Arc::clone(&realtime);
    let reply_deliveries = Arc::new(reply_delivery::ReplyDeliveryService::new(
        pool.clone(),
        Arc::clone(&runtime),
        Arc::new(move |event| delivery_realtime.broadcast(&event)),
    ));
    reply_deliveries.start();
    // Node처럼 롤플레이가 로컬 프로필을 쓰면 서버의 로컬 제공자로 문을 연다.
    let roleplay_realtime = Arc::clone(&realtime);
    let roleplay_doors: Arc<dyn roleplay::service::LocalDoors> = Arc::new(local_providers.clone());
    let roleplay = roleplay::RoleplayService::new(roleplay::service::RoleplayOptions::from_env(
        pool.clone(),
        Arc::new(move |event| roleplay_realtime.broadcast(&event)),
        Arc::new(move || Some(Arc::clone(&roleplay_doors))),
    )?);
    let slack_pool = pool.clone();
    // Node voiceFollow. 앱의 음성 지원 토글로 백엔드 실행 파일 옆의 따라 읽기 실행 파일을 켜고 끈다.
    let voice_follow = Arc::new(voice::VoiceFollowControl::with_process_follower());
    let state = AppState {
        pool,
        runtime: Arc::clone(&runtime),
        release_id: config.release_id.clone(),
        cost_summary,
        realtime: Arc::clone(&realtime),
        embedding: embedding.clone(),
        model_catalog: Some(Arc::clone(&model_catalog)),
        cli_updates: Arc::default(),
        usage_summary: Some(usage_summary),
        roleplay: Some(roleplay),
        voice_follow: Arc::clone(&voice_follow),
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port))
        .await
        .map_err(|error| format!("포트 {}을(를) 열 수 없습니다. {error}", config.port))?;
    println!("사무실 백엔드 실행 중 http://127.0.0.1:{}", config.port);
    // Node server.listen 콜백처럼 서버가 열린 뒤 Slack 연동을 따로 시작한다. 실패해도 백엔드는 계속 돈다.
    tokio::spawn(slack_bridge::serve_slack_bridge(slack_bridge::SlackBridgeOptions::new(
        slack_pool,
        format!("http://127.0.0.1:{}", config.port),
    )));
    let served = axum::serve(listener, http::router(state))
        // Node shutdown()의 socket.terminate()처럼 종료 신호를 받으면 열린 웹소켓을 닫는 프레임 없이 먼저 끊는다.
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Node shutdown()처럼 답변 전달 정리 전에 음성 따라 읽기를 멈춘다.
            voice_follow.stop();
            // Node shutdown()처럼 웹소켓을 끊기 전에 진행 중인 답변 전달 확인을 마친다.
            if let Err(message) = reply_deliveries.stop().await {
                eprintln!("백엔드 종료 중 오류가 발생했습니다. {message}");
            }
            // Node shutdown()처럼 답변 전달 다음에 열린 터미널을 닫아 진행 중 턴을 중단으로 남긴다.
            if let Err(error) = terminal_sessions.shutdown().await {
                eprintln!("백엔드 종료 중 오류가 발생했습니다. {error}");
            }
            // Node shutdown()처럼 터미널 다음에 로컬 모델을 정리한다.
            if let Err(error) = local_providers.shutdown().await {
                eprintln!("백엔드 종료 중 오류가 발생했습니다. {error}");
            }
            realtime.terminate();
        })
        .await
        .map_err(|error| error.to_string());
    pricing.stop();
    model_catalog.stop();
    runtime.shutdown();
    embedding.close();
    served
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(_) => {
            tokio::signal::ctrl_c().await.ok();
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}
