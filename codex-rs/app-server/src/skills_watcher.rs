use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SkillsChangedNotification;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::Receiver;
use codex_file_watcher::ThrottledWatchReceiver;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_skills::system_cache_root_dir;
use codex_skills_extension::HostSkillsLoadInput;
use codex_skills_extension::HostSkillsService;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::DropGuard;
use tracing::warn;

#[cfg(not(test))]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct SkillsWatcher {
    subscriber: FileWatcherSubscriber,
    skills_service: Arc<HostSkillsService>,
    outgoing: Arc<OutgoingMessageSender>,
    runtime_extra_roots_registration: Mutex<WatchRegistration>,
    shutdown_token: CancellationToken,
    _shutdown_drop_guard: DropGuard,
}

impl SkillsWatcher {
    pub(crate) fn new(
        skills_service: Arc<HostSkillsService>,
        codex_home: &AbsolutePathBuf,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Arc<Self> {
        let file_watcher = match FileWatcher::new() {
            Ok(file_watcher) => Arc::new(file_watcher),
            Err(err) => {
                warn!("failed to initialize skills file watcher: {err}");
                Arc::new(FileWatcher::noop())
            }
        };
        let (subscriber, rx) = file_watcher.add_subscriber();
        let shutdown_token = CancellationToken::new();
        let shutdown_drop_guard = shutdown_token.clone().drop_guard();
        let system_skills_root = system_cache_root_dir(codex_home);
        Self::spawn_event_loop(
            rx,
            Arc::clone(&skills_service),
            system_skills_root,
            Arc::clone(&outgoing),
            shutdown_token.child_token(),
        );
        Arc::new(Self {
            subscriber,
            skills_service,
            outgoing,
            runtime_extra_roots_registration: Mutex::new(WatchRegistration::default()),
            shutdown_token,
            _shutdown_drop_guard: shutdown_drop_guard,
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    pub(crate) fn register_runtime_extra_roots(&self, extra_roots: &[AbsolutePathBuf]) {
        let roots = extra_roots
            .iter()
            .map(|root| WatchPath {
                path: root.clone().into_path_buf(),
                recursive: true,
            })
            .collect();
        let registration = self.subscriber.register_paths(roots);
        let mut guard = self
            .runtime_extra_roots_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = registration;
    }

    pub(crate) fn watch_thread(
        self: &Arc<Self>,
        config: Arc<Config>,
        thread_manager: Arc<ThreadManager>,
        environments: Vec<TurnEnvironmentSelection>,
    ) -> DropGuard {
        let token = self.shutdown_token.child_token();
        let guard = token.clone().drop_guard();
        let watcher = Arc::clone(self);
        tokio::spawn(async move {
            let roots = tokio::select! {
                _ = token.cancelled() => return,
                roots = Self::thread_roots(&config, &thread_manager, &environments) => roots,
            };
            if roots.is_empty() || token.is_cancelled() {
                return;
            }
            // FSEvents registration and teardown can block on the OS service.
            // Keep both outside thread replies and the async event loop.
            let registration = tokio::task::spawn_blocking({
                let watcher = Arc::clone(&watcher);
                move || watcher.subscriber.register_paths(roots)
            })
            .await;
            let registration = match registration {
                Ok(registration) => registration,
                Err(err) => {
                    warn!("failed to register thread skills watcher: {err}");
                    return;
                }
            };
            // Changes during registration may precede the first watch event.
            tokio::select! {
                biased;
                _ = token.cancelled() => {},
                _ = Self::invalidate(&watcher.skills_service, &watcher.outgoing) => {
                    token.cancelled().await;
                },
            }
            let _ = tokio::task::spawn_blocking(move || drop(registration)).await;
        });
        guard
    }

    async fn thread_roots(
        config: &Config,
        thread_manager: &ThreadManager,
        environments: &[TurnEnvironmentSelection],
    ) -> Vec<WatchPath> {
        let Some(environment_selection) = environments.first() else {
            return Vec::new();
        };
        let Some(environment) = thread_manager
            .environment_manager()
            .get_environment(&environment_selection.environment_id)
        else {
            warn!(
                "failed to register skills watcher for unknown environment `{}`",
                environment_selection.environment_id
            );
            return Vec::new();
        };
        if environment.is_remote() {
            return Vec::new();
        }

        let plugins_input = config.plugins_config_input();
        let plugins_manager = thread_manager.plugins_manager();
        let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
        let skills_input = HostSkillsLoadInput::new(
            config.cwd.clone(),
            plugin_outcome.effective_plugin_skill_roots(),
            config.config_layer_stack.clone(),
        );
        thread_manager
            .skills_service()
            .watchable_skill_root_paths(&skills_input, environment.get_filesystem())
            .await
            .into_iter()
            .map(|path| WatchPath {
                path: path.into_path_buf(),
                recursive: true,
            })
            .collect()
    }

    async fn invalidate(skills_service: &HostSkillsService, outgoing: &OutgoingMessageSender) {
        skills_service.clear_cache();
        outgoing
            .send_server_notification(ServerNotification::SkillsChanged(
                SkillsChangedNotification {},
            ))
            .await;
    }

    fn spawn_event_loop(
        rx: Receiver,
        skills_service: Arc<HostSkillsService>,
        system_skills_root: AbsolutePathBuf,
        outgoing: Arc<OutgoingMessageSender>,
        shutdown_token: CancellationToken,
    ) {
        let mut rx = ThrottledWatchReceiver::new(rx, WATCHER_THROTTLE_INTERVAL);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("skills watcher listener skipped: no Tokio runtime available");
            return;
        };
        handle.spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = shutdown_token.cancelled() => break,
                    event = rx.recv() => event,
                };
                let Some(event) = event else {
                    break;
                };
                // The legacy user-skills root contains `.system` and is watched recursively.
                if event
                    .paths
                    .iter()
                    .all(|path| path.starts_with(system_skills_root.as_path()))
                {
                    continue;
                }
                Self::invalidate(&skills_service, &outgoing).await;
            }
        });
    }
}
