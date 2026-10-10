//! Open two repositories in one process with isolated environments and sinks.
//!
//! Source for the library guide "Embedding" page (included in the docs site).

use std::sync::Arc;

use grit_lib::command_runner::RecordingRunner;
use grit_lib::diagnostics::{CollectingDiagnostics, DiagnosticSink};
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::{init_repository, Repository};

fn main() -> Result<(), Error> {
    let base = tempfile::tempdir().map_err(Error::Io)?;

    let open_named =
        |name: &str, email: &str| -> Result<(Repository, Arc<CollectingDiagnostics>), Error> {
            let root = base.path().join(name);
            init_repository(
                &root,
                false,
                "main",
                None,
                grit_lib::RefStorageFormat::Files,
            )?;

            let home = base.path().join(format!("home-{name}"));
            std::fs::create_dir_all(&home).map_err(Error::Io)?;
            let global = home.join(".gitconfig");
            std::fs::write(
                &global,
                format!("[user]\n\tname = {name}\n\temail = {email}\n"),
            )
            .map_err(Error::Io)?;

            let mut env = Environment::empty();
            env.cwd = root.clone();
            env.home = Some(home.into());
            env.git_config_global = Some(global.to_string_lossy().into_owned());
            env.git_config_nosystem = Some("true".into());
            env.git_config_system = Some("/dev/null".into());

            let diagnostics = Arc::new(CollectingDiagnostics::new());
            let sink: Arc<dyn DiagnosticSink + Send + Sync> = diagnostics.clone();
            let runner = RecordingRunner::always_success();
            let mut options = RepositoryOptions::with_environment(env).with_command_runner(runner);
            options.diagnostics = sink;

            let git_dir = root.join(".git");
            let repo = Repository::open_with(&options, &git_dir, Some(&root))?;
            Ok((repo, diagnostics))
        };

    let (repo_a, sink_a) = open_named("alpha", "a@example.com")?;
    let (repo_b, sink_b) = open_named("beta", "b@example.com")?;

    let name_a = repo_a.config()?.get("user.name").unwrap_or_default();
    let name_b = repo_b.config()?.get("user.name").unwrap_or_default();
    println!("alpha user.name={name_a}");
    println!("beta user.name={name_b}");
    println!("alpha warnings={}", sink_a.warnings().len());
    println!("beta warnings={}", sink_b.warnings().len());

    Ok(())
}
