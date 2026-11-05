//! Prelude re-exports must also resolve at their canonical domain paths.

#[test]
fn prelude_items_resolve_at_domain_paths() {
    use grit_lib::prelude::*;

    use grit_lib::command_runner::{CommandRunner, RecordingRunner, SystemCommandRunner};
    use grit_lib::config::environment::{Environment, RepositoryOptions};
    use grit_lib::config::ConfigSet;
    use grit_lib::diagnostics::{
        CollectingDiagnostics, DiagnosticSink, NullDiagnostics, Trace, Warning,
    };
    use grit_lib::error::{Error, MidxError, Result};
    use grit_lib::index::Index;
    use grit_lib::objects::odb::Odb;
    use grit_lib::objects::{Object, ObjectId, ObjectInfo, ObjectKind};
    use grit_lib::ownership_check::{
        OwnershipCheck, OwnershipContext, OwnershipError, OwnershipStatus, SystemOwnershipCheck,
    };
    use grit_lib::repo::{
        resolve_initial_branch, DiscoveryInfo, InitOptions, InitOutcome, Repository,
        RepositoryBuilder,
    };

    let _ = (
        std::any::type_name::<RecordingRunner>(),
        std::any::type_name::<SystemCommandRunner>(),
        std::any::type_name::<ConfigSet>(),
        std::any::type_name::<CollectingDiagnostics>(),
        std::any::type_name::<NullDiagnostics>(),
        std::any::type_name::<Trace>(),
        std::any::type_name::<Warning>(),
        std::any::type_name::<Environment>(),
        std::any::type_name::<RepositoryOptions>(),
        std::any::type_name::<Error>(),
        std::any::type_name::<MidxError>(),
        std::any::type_name::<Result<()>>(),
        std::any::type_name::<Index>(),
        std::any::type_name::<Odb>(),
        std::any::type_name::<Object>(),
        std::any::type_name::<ObjectId>(),
        std::any::type_name::<ObjectInfo>(),
        std::any::type_name::<ObjectKind>(),
        std::any::type_name::<OwnershipContext>(),
        std::any::type_name::<OwnershipError>(),
        std::any::type_name::<OwnershipStatus>(),
        std::any::type_name::<SystemOwnershipCheck>(),
        std::any::type_name::<Repository>(),
        std::any::type_name::<RepositoryBuilder>(),
        std::any::type_name::<InitOptions>(),
        std::any::type_name::<InitOutcome>(),
        std::any::type_name::<DiscoveryInfo>(),
        std::any::type_name::<dyn CommandRunner>(),
        std::any::type_name::<dyn DiagnosticSink>(),
        std::any::type_name::<dyn OwnershipCheck>(),
    );
    let _ = resolve_initial_branch;
}
