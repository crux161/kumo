use alloc::vec::Vec;

use kumo_abi::{Handle, KoId, ObjectKind, Rights, Signals, INVALID_HANDLE};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectError {
    BadHandle,
    WrongType,
    AccessDenied,
    TableFull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelObject {
    koid: KoId,
    kind: ObjectKind,
    signals: Signals,
}

impl KernelObject {
    pub const fn new(koid: KoId, kind: ObjectKind) -> Self {
        Self {
            koid,
            kind,
            signals: Signals::empty(),
        }
    }

    pub const fn koid(self) -> KoId {
        self.koid
    }

    pub const fn kind(self) -> ObjectKind {
        self.kind
    }

    pub const fn signals(self) -> Signals {
        self.signals
    }

    pub fn signal(&mut self, signals: Signals) {
        self.signals |= signals;
    }
}

#[derive(Clone, Debug)]
pub struct ObjectManager {
    next_koid: u64,
}

impl ObjectManager {
    pub const fn new() -> Self {
        Self { next_koid: 1 }
    }

    pub fn create(&mut self, kind: ObjectKind) -> KernelObject {
        let object = KernelObject {
            koid: KoId(self.next_koid),
            kind,
            signals: Signals::empty(),
        };
        self.next_koid = self.next_koid.saturating_add(1).max(1);
        object
    }
}

impl Default for ObjectManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandleEntry {
    pub handle: Handle,
    pub koid: KoId,
    pub kind: ObjectKind,
    pub rights: Rights,
}

/// A handle grant staged in a target table but not yet committed in its source.
///
/// Staging makes `ProcessRun` transactional: admission failure can discard the
/// target handle while leaving a transfer-marked source handle untouched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StagedHandleGrant {
    source: Handle,
    target: Handle,
    transfer: bool,
}

impl StagedHandleGrant {
    pub const fn target(self) -> Handle {
        self.target
    }
}

#[derive(Clone, Debug)]
pub struct HandleTable {
    entries: Vec<Option<HandleEntry>>,
    next_handle: u32,
}

impl HandleTable {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_handle: 1,
        }
    }

    pub fn insert(&mut self, object: KernelObject, rights: Rights) -> Result<Handle, ObjectError> {
        self.insert_parts(object.koid, object.kind, rights)
    }

    pub fn get(&self, handle: Handle) -> Result<HandleEntry, ObjectError> {
        let index = handle_index(handle).ok_or(ObjectError::BadHandle)?;
        self.entries
            .get(index)
            .and_then(|slot| *slot)
            .ok_or(ObjectError::BadHandle)
    }

    pub fn require(
        &self,
        handle: Handle,
        kind: ObjectKind,
        rights: Rights,
    ) -> Result<HandleEntry, ObjectError> {
        let entry = self.get(handle)?;
        if entry.kind != kind {
            return Err(ObjectError::WrongType);
        }
        if !entry.rights.contains(rights) {
            return Err(ObjectError::AccessDenied);
        }
        Ok(entry)
    }

    pub fn duplicate(&mut self, handle: Handle, rights: Rights) -> Result<Handle, ObjectError> {
        let entry = self.get(handle)?;
        if !entry.rights.contains(Rights::DUPLICATE) || !entry.rights.contains(rights) {
            return Err(ObjectError::AccessDenied);
        }
        self.insert_parts(entry.koid, entry.kind, rights)
    }

    pub fn close(&mut self, handle: Handle) -> Result<(), ObjectError> {
        self.remove(handle).map(|_| ())
    }

    pub fn remove(&mut self, handle: Handle) -> Result<HandleEntry, ObjectError> {
        let index = handle_index(handle).ok_or(ObjectError::BadHandle)?;
        let slot = self.entries.get_mut(index).ok_or(ObjectError::BadHandle)?;
        slot.take().ok_or(ObjectError::BadHandle)
    }

    pub fn live_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }

    pub fn drain(&mut self) -> Vec<HandleEntry> {
        let mut removed = Vec::new();
        for slot in &mut self.entries {
            if let Some(entry) = slot.take() {
                removed.push(entry);
            }
        }
        removed
    }

    pub fn insert_entry(&mut self, entry: HandleEntry) -> Result<Handle, ObjectError> {
        self.insert_parts(entry.koid, entry.kind, entry.rights)
    }

    /// Stage a copied or moved grant in `target` without changing this table.
    pub fn stage_grant_to(
        &self,
        target: &mut Self,
        source: Handle,
        transfer: bool,
    ) -> Result<StagedHandleGrant, ObjectError> {
        let entry = self.get(source)?;
        let required = if transfer {
            Rights::TRANSFER
        } else {
            Rights::DUPLICATE
        };
        if !entry.rights.contains(required) {
            return Err(ObjectError::AccessDenied);
        }
        let target = target.insert_entry(entry)?;
        Ok(StagedHandleGrant {
            source,
            target,
            transfer,
        })
    }

    /// Commit a staged grant in its source table.
    pub fn commit_grant(&mut self, grant: StagedHandleGrant) -> Result<(), ObjectError> {
        if grant.transfer {
            self.remove(grant.source)?;
        }
        Ok(())
    }

    /// Roll a staged grant back out of its target table.
    pub fn rollback_grant(&mut self, grant: StagedHandleGrant) -> Result<(), ObjectError> {
        self.remove(grant.target).map(|_| ())
    }

    fn insert_parts(
        &mut self,
        koid: KoId,
        kind: ObjectKind,
        rights: Rights,
    ) -> Result<Handle, ObjectError> {
        if self.next_handle == 0 {
            return Err(ObjectError::TableFull);
        }

        let handle = Handle(self.next_handle);
        self.next_handle = self.next_handle.checked_add(1).unwrap_or(0);
        self.entries.push(Some(HandleEntry {
            handle,
            koid,
            kind,
            rights,
        }));
        Ok(handle)
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

fn handle_index(handle: Handle) -> Option<usize> {
    if handle == INVALID_HANDLE {
        None
    } else {
        Some((handle.0 - 1) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_ids_are_monotonic_and_nonzero() {
        let mut objects = ObjectManager::new();
        let job = objects.create(ObjectKind::Job);
        let process = objects.create(ObjectKind::Process);
        assert_eq!(job.koid(), KoId(1));
        assert_eq!(process.koid(), KoId(2));
        assert_eq!(job.kind(), ObjectKind::Job);
    }

    #[test]
    fn handles_enforce_kind_and_rights() {
        let mut objects = ObjectManager::new();
        let vmo = objects.create(ObjectKind::Vmo);
        let mut handles = HandleTable::new();
        let handle = handles
            .insert(vmo, Rights::READ | Rights::WRITE | Rights::DUPLICATE)
            .unwrap();

        assert!(handles
            .require(handle, ObjectKind::Vmo, Rights::READ)
            .is_ok());
        assert_eq!(
            handles.require(handle, ObjectKind::Channel, Rights::READ),
            Err(ObjectError::WrongType)
        );
        assert_eq!(
            handles.require(handle, ObjectKind::Vmo, Rights::MAP),
            Err(ObjectError::AccessDenied)
        );
    }

    #[test]
    fn duplicate_can_only_narrow_rights() {
        let mut objects = ObjectManager::new();
        let channel = objects.create(ObjectKind::Channel);
        let mut handles = HandleTable::new();
        let handle = handles
            .insert(channel, Rights::READ | Rights::WRITE | Rights::DUPLICATE)
            .unwrap();

        let read_only = handles.duplicate(handle, Rights::READ).unwrap();
        assert!(handles
            .require(read_only, ObjectKind::Channel, Rights::READ)
            .is_ok());
        assert_eq!(
            handles.require(read_only, ObjectKind::Channel, Rights::WRITE),
            Err(ObjectError::AccessDenied)
        );
        assert_eq!(
            handles.duplicate(handle, Rights::READ | Rights::MAP),
            Err(ObjectError::AccessDenied)
        );
    }

    #[test]
    fn close_invalidates_the_process_local_handle() {
        let mut objects = ObjectManager::new();
        let event = objects.create(ObjectKind::Event);
        let mut handles = HandleTable::new();
        let handle = handles.insert(event, Rights::WAIT).unwrap();
        assert_eq!(handles.live_count(), 1);
        handles.close(handle).unwrap();
        assert_eq!(handles.live_count(), 0);
        assert_eq!(handles.get(handle), Err(ObjectError::BadHandle));
        assert_eq!(handles.close(handle), Err(ObjectError::BadHandle));
    }

    #[test]
    fn remove_returns_the_handle_entry_for_transfer() {
        let mut objects = ObjectManager::new();
        let channel = objects.create(ObjectKind::Channel);
        let mut handles = HandleTable::new();
        let handle = handles
            .insert(channel, Rights::READ | Rights::TRANSFER)
            .unwrap();

        let entry = handles.remove(handle).unwrap();

        assert_eq!(entry.koid, channel.koid());
        assert_eq!(entry.kind, ObjectKind::Channel);
        assert_eq!(entry.rights, Rights::READ | Rights::TRANSFER);
        assert_eq!(handles.get(handle), Err(ObjectError::BadHandle));
    }

    #[test]
    fn staged_grants_copy_move_and_rollback_transactionally() {
        let mut objects = ObjectManager::new();
        let channel = objects.create(ObjectKind::Channel);
        let rights = Rights::READ | Rights::DUPLICATE | Rights::TRANSFER;
        let mut source = HandleTable::new();
        let copied = source.insert(channel, rights).unwrap();
        let moved = source.insert(channel, rights).unwrap();
        let rolled_back = source.insert(channel, rights).unwrap();
        let mut target = HandleTable::new();

        let copy_grant = source.stage_grant_to(&mut target, copied, false).unwrap();
        source.commit_grant(copy_grant).unwrap();
        assert!(source.get(copied).is_ok());
        assert!(target.get(copy_grant.target()).is_ok());

        let move_grant = source.stage_grant_to(&mut target, moved, true).unwrap();
        source.commit_grant(move_grant).unwrap();
        assert_eq!(source.get(moved), Err(ObjectError::BadHandle));
        assert!(target.get(move_grant.target()).is_ok());

        let rollback_grant = source
            .stage_grant_to(&mut target, rolled_back, true)
            .unwrap();
        target.rollback_grant(rollback_grant).unwrap();
        assert!(source.get(rolled_back).is_ok());
        assert_eq!(
            target.get(rollback_grant.target()),
            Err(ObjectError::BadHandle)
        );
    }

    #[test]
    fn staged_grants_enforce_copy_and_transfer_rights() {
        let mut objects = ObjectManager::new();
        let event = objects.create(ObjectKind::Event);
        let mut source = HandleTable::new();
        let copy_only = source.insert(event, Rights::DUPLICATE).unwrap();
        let transfer_only = source.insert(event, Rights::TRANSFER).unwrap();
        let mut target = HandleTable::new();

        assert_eq!(
            source.stage_grant_to(&mut target, copy_only, true),
            Err(ObjectError::AccessDenied)
        );
        assert_eq!(
            source.stage_grant_to(&mut target, transfer_only, false),
            Err(ObjectError::AccessDenied)
        );
        assert_eq!(target.live_count(), 0);
    }

    #[test]
    fn signals_are_real_object_state() {
        let mut objects = ObjectManager::new();
        let mut thread = objects.create(ObjectKind::Thread);
        thread.signal(Signals::TERMINATED);
        assert!(thread.signals().contains(Signals::TERMINATED));
        assert!(!thread.signals().contains(Signals::IRQ));
    }
}
