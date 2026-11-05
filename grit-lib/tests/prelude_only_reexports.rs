//! Prelude re-exports must also resolve at their canonical domain paths.

#[test]
fn prelude_items_resolve_at_domain_paths() {
    use grit_lib::prelude::{
        resolve_initial_branch as prelude_resolve_initial_branch,
        CollectingDiagnostics as PreludeCollectingDiagnostics,
        CommandRunner as PreludeCommandRunner, ConfigSet as PreludeConfigSet,
        DiagnosticSink as PreludeDiagnosticSink, DiscoveryInfo as PreludeDiscoveryInfo,
        Environment as PreludeEnvironment, Error as PreludeError, Index as PreludeIndex,
        InitOptions as PreludeInitOptions, InitOutcome as PreludeInitOutcome,
        MidxError as PreludeMidxError, NullDiagnostics as PreludeNullDiagnostics,
        Object as PreludeObject, ObjectId as PreludeObjectId, ObjectInfo as PreludeObjectInfo,
        ObjectKind as PreludeObjectKind, Odb as PreludeOdb,
        OwnershipCheck as PreludeOwnershipCheck, OwnershipContext as PreludeOwnershipContext,
        OwnershipError as PreludeOwnershipError, OwnershipStatus as PreludeOwnershipStatus,
        RecordingRunner as PreludeRecordingRunner, Repository as PreludeRepository,
        RepositoryBuilder as PreludeRepositoryBuilder,
        RepositoryOptions as PreludeRepositoryOptions, Result as PreludeResult,
        SystemCommandRunner as PreludeSystemCommandRunner,
        SystemOwnershipCheck as PreludeSystemOwnershipCheck, Trace as PreludeTrace,
        Warning as PreludeWarning,
    };

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

    fn assert_same<T, U>() {
        assert_eq!(std::any::type_name::<T>(), std::any::type_name::<U>());
    }

    assert_same::<PreludeConfigSet, ConfigSet>();
    assert_same::<PreludeCollectingDiagnostics, CollectingDiagnostics>();
    assert_same::<PreludeNullDiagnostics, NullDiagnostics>();
    assert_same::<PreludeTrace, Trace>();
    assert_same::<PreludeWarning, Warning>();
    assert_same::<PreludeEnvironment, Environment>();
    assert_same::<PreludeRepositoryOptions, RepositoryOptions>();
    assert_same::<PreludeError, Error>();
    assert_same::<PreludeMidxError, MidxError>();
    assert_same::<PreludeResult<()>, Result<()>>();
    assert_same::<PreludeIndex, Index>();
    assert_same::<PreludeOdb, Odb>();
    assert_same::<PreludeObject, Object>();
    assert_same::<PreludeObjectId, ObjectId>();
    assert_same::<PreludeObjectInfo, ObjectInfo>();
    assert_same::<PreludeObjectKind, ObjectKind>();
    assert_same::<PreludeOwnershipContext, OwnershipContext>();
    assert_same::<PreludeOwnershipError, OwnershipError>();
    assert_same::<PreludeOwnershipStatus, OwnershipStatus>();
    assert_same::<PreludeSystemOwnershipCheck, SystemOwnershipCheck>();
    assert_same::<PreludeRecordingRunner, RecordingRunner>();
    assert_same::<PreludeSystemCommandRunner, SystemCommandRunner>();
    assert_same::<PreludeRepository, Repository>();
    assert_same::<PreludeRepositoryBuilder, RepositoryBuilder>();
    assert_same::<PreludeInitOptions, InitOptions>();
    assert_same::<PreludeInitOutcome, InitOutcome>();
    assert_same::<PreludeDiscoveryInfo, DiscoveryInfo>();

    assert_eq!(
        std::any::type_name::<dyn PreludeCommandRunner>(),
        std::any::type_name::<dyn CommandRunner>()
    );
    assert_eq!(
        std::any::type_name::<dyn PreludeDiagnosticSink>(),
        std::any::type_name::<dyn DiagnosticSink>()
    );
    assert_eq!(
        std::any::type_name::<dyn PreludeOwnershipCheck>(),
        std::any::type_name::<dyn OwnershipCheck>()
    );

    type ResolveInitialBranch = fn(&Environment, Option<&str>) -> grit_lib::error::Result<String>;
    let from_prelude: ResolveInitialBranch = prelude_resolve_initial_branch;
    let from_domain: ResolveInitialBranch = resolve_initial_branch;
    assert_eq!(from_prelude as *const (), from_domain as *const ());

    let _ = std::any::type_name::<PreludeRepository>();
}
