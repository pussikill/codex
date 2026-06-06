use std::sync::Arc;

use anyhow::Result;
use codex_core::ForkSnapshot;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::GlobalInstruction;
use codex_extension_api::GlobalInstructions;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::responses;
use core_test_support::test_codex::TestGlobalInstructionsContributor;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn instructions(
    contents: &str,
    source: Option<AbsolutePathBuf>,
    warnings: &[&str],
) -> GlobalInstructions {
    GlobalInstructions {
        instructions: vec![GlobalInstruction::new(contents, source)],
        warnings: warnings
            .iter()
            .map(|warning| (*warning).to_string())
            .collect(),
    }
}

fn extensions(
    contributor: Arc<TestGlobalInstructionsContributor>,
) -> Arc<ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::new();
    builder.global_instructions_contributor(contributor);
    Arc::new(builder.build())
}

fn user_instructions(request: &responses::ResponsesRequest) -> String {
    let Some(instructions) = request
        .message_input_texts("user")
        .into_iter()
        .find(|text| text.starts_with("# AGENTS.md instructions for "))
    else {
        panic!("global instructions message");
    };
    instructions
}

fn local_compaction_provider(server: &wiremock::MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.name = "OpenAI-compatible test provider".to_string();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_thread_resolves_once_and_composes_global_before_project() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resp-2"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let source_dir = TempDir::new()?;
    let global_source = AbsolutePathBuf::try_from(source_dir.path().join("AGENTS.md"))?;
    let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![Ok(
        instructions("global instructions", Some(global_source.clone()), &[]),
    )]));

    let mut builder = test_codex()
        .with_extensions(extensions(Arc::clone(&contributor)))
        .with_workspace_setup(|cwd, fs| async move {
            fs.write_file(
                &cwd.join("AGENTS.md"),
                b"project instructions".to_vec(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let test = builder.build_with_remote_env(&server).await?;

    assert_eq!(
        test.codex.instruction_sources().await,
        vec![global_source, test.config.cwd.join("AGENTS.md")]
    );

    test.submit_turn("first turn").await?;
    test.submit_turn("second turn").await?;

    assert_eq!(contributor.calls(), 1);
    let requests = response_mock.requests();
    let rendered = user_instructions(&requests[0]);
    assert!(
        rendered.find("global instructions") < rendered.find("project instructions"),
        "global instructions should precede project instructions: {rendered}"
    );
    assert!(
        rendered.contains("--- project-doc ---"),
        "global/project boundary should retain the project separator: {rendered}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_warnings_surface_and_failure_aborts_thread_creation() -> Result<()> {
    let server = responses::start_mock_server().await;
    let warning_contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![Ok(
        instructions("global", None, &["global warning"]),
    )]));
    let mut warning_builder =
        test_codex().with_extensions(extensions(Arc::clone(&warning_contributor)));
    let warning_test = warning_builder.build(&server).await?;

    let warning = wait_for_event_match(&warning_test.codex, |event| match event {
        EventMsg::Warning(warning) if warning.message == "global warning" => {
            Some(warning.message.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(warning, "global warning");
    assert_eq!(warning_contributor.calls(), 1);

    let failure_contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![Err(
        "contributor failed".to_string(),
    )]));
    let mut failure_builder =
        test_codex().with_extensions(extensions(Arc::clone(&failure_contributor)));
    let error = match failure_builder.build(&server).await {
        Ok(_) => panic!("thread creation should fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("contributor failed"),
        "unexpected creation error: {error:#}"
    );
    assert_eq!(failure_contributor.calls(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_resume_inherits_persisted_snapshot_without_resolution() -> Result<()> {
    let server = responses::start_mock_server().await;
    let initial_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("initial-response"),
            responses::ev_completed("initial-response"),
        ]),
    )
    .await;
    let source_dir = TempDir::new()?;
    let old_source = AbsolutePathBuf::try_from(source_dir.path().join("old.md"))?;
    let new_source = AbsolutePathBuf::try_from(source_dir.path().join("new.md"))?;
    let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![
        Ok(instructions(
            "old global instructions",
            Some(old_source.clone()),
            &[],
        )),
        Ok(instructions(
            "new global instructions",
            Some(new_source),
            &[],
        )),
    ]));
    let registry = extensions(Arc::clone(&contributor));
    let mut initial_builder = test_codex().with_extensions(Arc::clone(&registry));
    let initial = initial_builder.build(&server).await?;
    initial.submit_turn("persist instructions").await?;
    assert!(user_instructions(&initial_mock.single_request()).contains("old global instructions"));
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");
    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    let resumed_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resumed-response"),
            responses::ev_completed("resumed-response"),
        ]),
    )
    .await;
    let mut resume_builder = test_codex().with_extensions(registry);
    let resumed = resume_builder
        .resume(&server, Arc::clone(&initial.home), rollout_path)
        .await?;

    assert_eq!(contributor.calls(), 1);
    assert_eq!(resumed.codex.instruction_sources().await, vec![old_source]);

    resumed.submit_turn("resume without refreshing").await?;

    assert_eq!(contributor.calls(), 1);
    let resumed_rendered = user_instructions(&resumed_mock.single_request());
    assert!(resumed_rendered.contains("old global instructions"));
    assert!(!resumed_rendered.contains("new global instructions"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_inherits_persisted_snapshot_without_resolution() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("parent-response"),
                responses::ev_completed("parent-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("fork-response"),
                responses::ev_completed("fork-response"),
            ]),
        ],
    )
    .await;
    let source_dir = TempDir::new()?;
    let old_source = AbsolutePathBuf::try_from(source_dir.path().join("old.md"))?;
    let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![
        Ok(instructions(
            "old global instructions",
            Some(old_source.clone()),
            &[],
        )),
        Ok(instructions("new global instructions", None, &[])),
    ]));
    let mut builder = test_codex().with_extensions(extensions(Arc::clone(&contributor)));
    let parent = builder.build(&server).await?;
    parent.submit_turn("persist instructions").await?;
    parent.codex.ensure_rollout_materialized().await;
    parent.codex.flush_rollout().await?;
    let rollout_path = parent.codex.rollout_path().expect("rollout path");

    let forked = parent
        .thread_manager
        .fork_thread(
            ForkSnapshot::Interrupted,
            parent.config.clone(),
            rollout_path,
            /*thread_source*/ None,
            /*parent_trace*/ None,
        )
        .await?;

    assert_eq!(contributor.calls(), 1);
    assert_eq!(forked.thread.instruction_sources().await, vec![old_source]);

    forked
        .thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "continue fork".to_string(),
                text_elements: Vec::new(),
            }],
            environments: None,
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&forked.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_eq!(contributor.calls(), 1);
    let rendered = user_instructions(&response_mock.requests()[1]);
    assert!(rendered.contains("old global instructions"));
    assert!(!rendered.contains("new global instructions"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_without_structured_snapshot_does_not_resolve_contributor() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("initial-response"),
                responses::ev_completed("initial-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resumed-response"),
                responses::ev_completed("resumed-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("compact-response"),
                responses::ev_assistant_message("compact-message", "summary"),
                responses::ev_completed("compact-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("post-compact-response"),
                responses::ev_completed("post-compact-response"),
            ]),
        ],
    )
    .await;
    let provider = local_compaction_provider(&server);
    let mut initial_builder = test_codex()
        .with_config({
            let provider = provider.clone();
            move |config| config.model_provider = provider
        })
        .with_workspace_setup(|cwd, fs| async move {
            fs.write_file(
                &cwd.join("AGENTS.md"),
                b"legacy project instructions".to_vec(),
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        });
    let initial = initial_builder.build_with_remote_env(&server).await?;
    initial.submit_turn("persist legacy-style history").await?;
    let initial_cwd = initial.config.cwd.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");
    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let legacy_rollout = std::fs::read_to_string(&rollout_path)?
        .lines()
        .map(|line| {
            let mut rollout_line: RolloutLine = serde_json::from_str(line)?;
            if let RolloutItem::TurnContext(turn_context) = &mut rollout_line.item {
                turn_context.user_instructions = None;
            }
            serde_json::to_string(&rollout_line)
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?
        .join("\n");
    std::fs::write(&rollout_path, format!("{legacy_rollout}\n"))?;

    let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![Ok(
        instructions("new global instructions", None, &[]),
    )]));
    let mut resume_builder = test_codex()
        .with_extensions(extensions(Arc::clone(&contributor)))
        .with_config(move |config| {
            config.model_provider = provider;
            config.cwd = initial_cwd;
        });
    let resumed = resume_builder
        .resume(&server, Arc::clone(&initial.home), rollout_path)
        .await?;

    assert_eq!(contributor.calls(), 0);
    assert_eq!(
        resumed.codex.instruction_sources().await,
        Vec::<AbsolutePathBuf>::new()
    );

    resumed.submit_turn("do not resolve on resume").await?;

    assert_eq!(contributor.calls(), 0);
    assert!(
        !response_mock.requests()[1]
            .body_json()
            .to_string()
            .contains("new global instructions")
    );

    resumed.codex.submit(Op::Compact).await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(contributor.calls(), 0);

    resumed
        .submit_turn("resolve during the next full rebuild")
        .await?;

    assert_eq!(contributor.calls(), 1);
    let rebuilt = user_instructions(&response_mock.requests()[3]);
    assert!(rebuilt.contains("new global instructions"));
    assert!(rebuilt.contains("legacy project instructions"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_compaction_defers_refresh_until_next_full_context_injection() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("first-response"),
                responses::ev_completed("first-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("compact-response"),
                responses::ev_assistant_message("compact-message", "summary"),
                responses::ev_completed("compact-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("follow-up-response"),
                responses::ev_completed("follow-up-response"),
            ]),
        ],
    )
    .await;
    let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![
        Ok(instructions("old global instructions", None, &[])),
        Ok(instructions("new global instructions", None, &[])),
    ]));
    let provider = local_compaction_provider(&server);
    let mut builder = test_codex()
        .with_extensions(extensions(Arc::clone(&contributor)))
        .with_config(move |config| {
            config.model_provider = provider;
        });
    let test = builder.build(&server).await?;

    test.submit_turn("first turn").await?;
    assert_eq!(contributor.calls(), 1);

    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(contributor.calls(), 1);

    test.submit_turn("after compact").await?;

    assert_eq!(contributor.calls(), 2);
    let requests = response_mock.requests();
    let follow_up = user_instructions(&requests[2]);
    assert!(follow_up.contains("new global instructions"));
    assert!(!follow_up.contains("old global instructions"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_manual_compaction_preserves_the_deferred_refresh() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("first-response"),
                responses::ev_completed("first-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("compact-response"),
                responses::ev_assistant_message("compact-message", "summary"),
                responses::ev_completed("compact-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resumed-response"),
                responses::ev_completed("resumed-response"),
            ]),
        ],
    )
    .await;
    let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![
        Ok(instructions("old global instructions", None, &[])),
        Ok(instructions("new global instructions", None, &[])),
    ]));
    let provider = local_compaction_provider(&server);
    let mut initial_builder = test_codex()
        .with_extensions(extensions(Arc::clone(&contributor)))
        .with_config({
            let provider = provider.clone();
            move |config| config.model_provider = provider
        });
    let initial = initial_builder.build(&server).await?;
    initial.submit_turn("first turn").await?;
    initial.codex.submit(Op::Compact).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");
    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    let mut resume_builder = test_codex()
        .with_extensions(extensions(Arc::clone(&contributor)))
        .with_config(move |config| config.model_provider = provider);
    let resumed = resume_builder
        .resume(&server, Arc::clone(&initial.home), rollout_path)
        .await?;

    assert_eq!(contributor.calls(), 1);
    resumed.submit_turn("after resume").await?;

    assert_eq!(contributor.calls(), 2);
    let resumed_request = user_instructions(&response_mock.requests()[2]);
    assert!(resumed_request.contains("new global instructions"));
    assert!(!resumed_request.contains("old global instructions"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_turn_compaction_refreshes_and_failure_retains_previous_snapshot() -> Result<()> {
    for (refresh, expected, warning) in [
        (
            Ok(instructions(
                "new global instructions",
                None,
                &["refresh warning"],
            )),
            "new global instructions",
            Some("refresh warning"),
        ),
        (
            Err("refresh failed".to_string()),
            "old global instructions",
            Some("refresh failed"),
        ),
    ] {
        let server = responses::start_mock_server().await;
        let response_mock = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_function_call("call-1", "unsupported_tool", "{}"),
                    responses::ev_completed_with_tokens("first-response", /*total_tokens*/ 96),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("compact-message", "summary"),
                    responses::ev_completed_with_tokens(
                        "compact-response",
                        /*total_tokens*/ 10,
                    ),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("final-message", "done"),
                    responses::ev_completed_with_tokens(
                        "follow-up-response",
                        /*total_tokens*/ 10,
                    ),
                ]),
            ],
        )
        .await;
        let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![
            Ok(instructions("old global instructions", None, &[])),
            refresh,
        ]));
        let provider = local_compaction_provider(&server);
        let mut builder = test_codex()
            .with_extensions(extensions(Arc::clone(&contributor)))
            .with_config(move |config| {
                config.model_provider = provider;
                config.model_context_window = Some(100);
                config.model_auto_compact_token_limit = Some(90);
            });
        let test = builder.build(&server).await?;

        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "trigger mid-turn compaction".to_string(),
                    text_elements: Vec::new(),
                }],
                environments: None,
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await?;
        if let Some(warning) = warning {
            let actual = wait_for_event_match(&test.codex, |event| match event {
                EventMsg::Warning(event) if event.message.contains(warning) => {
                    Some(event.message.clone())
                }
                _ => None,
            })
            .await;
            assert!(actual.contains(warning));
        }
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;

        assert_eq!(contributor.calls(), 2);
        let continuation = user_instructions(&response_mock.requests()[2]);
        assert!(continuation.contains(expected));
        if expected == "old global instructions" {
            assert!(!continuation.contains("new global instructions"));
        } else {
            assert!(!continuation.contains("old global instructions"));
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_turn_refresh_snapshot_survives_cold_resume() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_function_call("call-1", "unsupported_tool", "{}"),
                responses::ev_completed_with_tokens("first-response", /*total_tokens*/ 96),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("compact-message", "summary"),
                responses::ev_completed_with_tokens("compact-response", /*total_tokens*/ 10),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("final-message", "done"),
                responses::ev_completed_with_tokens("follow-up-response", /*total_tokens*/ 10),
            ]),
            responses::sse(vec![
                responses::ev_response_created("resumed-response"),
                responses::ev_completed("resumed-response"),
            ]),
        ],
    )
    .await;
    let contributor = Arc::new(TestGlobalInstructionsContributor::new(vec![
        Ok(instructions("old global instructions", None, &[])),
        Ok(instructions("new global instructions", None, &[])),
        Ok(instructions("unexpected instructions", None, &[])),
    ]));
    let provider = local_compaction_provider(&server);
    let mut initial_builder = test_codex()
        .with_extensions(extensions(Arc::clone(&contributor)))
        .with_config({
            let provider = provider.clone();
            move |config| {
                config.model_provider = provider;
                config.model_context_window = Some(100);
                config.model_auto_compact_token_limit = Some(90);
            }
        });
    let initial = initial_builder.build(&server).await?;
    initial.submit_turn("trigger mid-turn compaction").await?;
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");
    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    let mut resume_builder = test_codex()
        .with_extensions(extensions(Arc::clone(&contributor)))
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_context_window = Some(100);
            config.model_auto_compact_token_limit = Some(90);
        });
    let resumed = resume_builder
        .resume(&server, Arc::clone(&initial.home), rollout_path)
        .await?;
    resumed.submit_turn("continue resumed thread").await?;

    assert_eq!(contributor.calls(), 2);
    let resumed_request = response_mock.requests()[3].body_json().to_string();
    assert!(resumed_request.contains("new global instructions"));
    assert!(!resumed_request.contains("old global instructions"));
    assert!(!resumed_request.contains("unexpected instructions"));

    Ok(())
}
