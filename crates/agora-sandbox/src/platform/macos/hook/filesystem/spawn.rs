use super::super::process::file_actions::Action;
use super::*;

pub(in crate::platform::hook) struct SpawnLocalDescriptors {
    pub(in crate::platform::hook) environment: Option<InheritedLocalEnvironment>,
    pub(in crate::platform::hook) retained: Option<SpawnLocalRetain>,
    auxiliaries: Vec<OwnedFd>,
    // Keep the original private descriptors alive until native spawn finishes;
    // their numbers must not be recycled while the child actions close them.
    _opens: Vec<Arc<OpenFile>>,
    pub(in crate::platform::hook) close: Vec<i32>,
}

impl SpawnLocalDescriptors {
    pub(in crate::platform::hook) fn descriptors(&self) -> impl Iterator<Item = i32> + '_ {
        self.auxiliaries.iter().map(AsRawFd::as_raw_fd)
    }
}

pub(in crate::platform::hook) fn prepare_local_spawn(
    actions: Option<&[Action]>,
    cloexec_default: bool,
    replaces_process: bool,
) -> Result<SpawnLocalDescriptors> {
    let mut prepared = SpawnLocalDescriptors {
        environment: None,
        retained: None,
        auxiliaries: Vec::new(),
        _opens: Vec::new(),
        close: Vec::new(),
    };
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return Ok(prepared);
    };
    let Some(local) = runtime.local.as_ref() else {
        return Ok(prepared);
    };
    let _guard =
        FilesystemHookGuard::enter().context("filesystem hook unavailable during spawn")?;
    let _operation = runtime
        .operations
        .acquire(mapping::OperationRequest::new().descriptor_registry_exclusive());
    let files = lock(&runtime.open_files);
    let has_local_descriptors = files
        .values()
        .any(|open| open.local_inheritance().is_some());
    if !has_local_descriptors && !replaces_process {
        return Ok(prepared);
    }
    let actions = match actions {
        Some(actions) => actions,
        None if !has_local_descriptors => &[],
        None => return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into()),
    };
    let mut aliases = HashMap::new();
    let mut originals = HashSet::new();
    for (&fd, open) in files.iter() {
        let Some(local) = open.local_inheritance() else {
            continue;
        };
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error().into());
        }
        aliases.insert(
            fd,
            (
                fd,
                Arc::clone(open),
                !cloexec_default && flags & libc::FD_CLOEXEC == 0,
            ),
        );
        originals.insert(local.state.as_raw_fd());
        originals.insert(local.lock.as_raw_fd());
        prepared._opens.push(Arc::clone(open));
    }
    drop(files);
    for action in actions {
        match action {
            Action::Close(fd) | Action::Open(fd, ..) => {
                aliases.remove(fd);
                originals.remove(fd);
            }
            Action::Dup2(source, destination) => {
                match aliases.get(source).cloned() {
                    Some((source, open, _)) => {
                        aliases.insert(*destination, (source, open, true));
                    }
                    None => {
                        aliases.remove(destination);
                    }
                }
                originals.remove(destination);
            }
            Action::Inherit(fd) => {
                if let Some((_, _, inherits)) = aliases.get_mut(fd) {
                    *inherits = true;
                }
                originals.remove(fd);
            }
            Action::Chdir(_) | Action::Fchdir(_) => {}
        }
    }
    prepared.close = originals.into_iter().collect();
    // Avoid caller action operands without requiring a descriptor above their
    // maximum: a valid action may already refer to RLIMIT_NOFILE - 1.
    let reserved = actions
        .iter()
        .flat_map(Action::descriptors)
        .chain(aliases.keys().copied())
        .collect::<HashSet<_>>();
    let mut minimum = 3;
    let mut copies = HashMap::new();
    let mut descriptors = Vec::new();
    for (fd, (source, open, inherits)) in aliases {
        if !inherits {
            continue;
        }
        let mut inherited = InheritedLocalDescriptor::capture(source, &open)?;
        inherited.descriptor = fd;
        for auxiliary in [
            &mut inherited.state_descriptor,
            &mut inherited.lock_descriptor,
        ] {
            let copy = if let Some(copy) = copies.get(auxiliary) {
                *copy
            } else {
                // Darwin SETEXEC closes CLOEXEC descriptors before its file
                // actions, unlike ordinary spawn. Only these private temporary
                // copies become inheritable; parent application flags stay intact.
                let command = if replaces_process {
                    libc::F_DUPFD
                } else {
                    libc::F_DUPFD_CLOEXEC
                };
                let copy = loop {
                    let fd = unsafe { libc::fcntl(*auxiliary, command, minimum) };
                    if fd < 0 {
                        return Err(io::Error::last_os_error().into());
                    }
                    let copy = unsafe { OwnedFd::from_raw_fd(fd) };
                    minimum = fd + 1;
                    if !reserved.contains(&fd) {
                        break copy;
                    }
                };
                let fd = copy.as_raw_fd();
                prepared.auxiliaries.push(copy);
                copies.insert(*auxiliary, fd);
                fd
            };
            *auxiliary = copy;
        }
        descriptors.push(inherited);
    }
    anyhow::ensure!(
        descriptors.len() <= MAX_INHERITED_LOCAL_DESCRIPTORS,
        "too many inherited local descriptors"
    );
    let mut handles = if replaces_process {
        runtime.local_handles()
    } else {
        Vec::new()
    };
    handles.extend(
        descriptors
            .iter()
            .map(|descriptor| descriptor.handle.clone()),
    );
    if handles.is_empty() {
        return Ok(prepared);
    }
    handles.sort_unstable();
    handles.dedup();
    let encoded = serde_json::to_string(&InheritedLocalDescriptors {
        version: INHERITED_LOCAL_DESCRIPTOR_VERSION,
        handles: handles.clone(),
        descriptors,
    })?;
    let environment = InheritedLocalEnvironment { encoded, handles };
    if !replaces_process {
        local.retain(environment.handles.clone())?;
        prepared.retained = Some(SpawnLocalRetain {
            release: Some((local.clone(), environment.handles.clone())),
        });
    }
    prepared.environment = Some(environment);
    Ok(prepared)
}
