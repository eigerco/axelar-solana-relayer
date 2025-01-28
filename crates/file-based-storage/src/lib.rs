#![expect(clippy::allow_attributes_without_reason)]

//! simple memory storage implementation using memory maps
use std::fs::OpenOptions;
use std::io::{self, Seek as _, SeekFrom, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};

use amplifier_api::types::{uuid, TaskItemId};
use bytemuck::{Pod, Zeroable};
use memmap2::MmapMut;

/// Memory map wrapper that implements the state to successfully store and retrieve latest task item
/// id
#[derive(Debug, Clone)]
pub struct MemmapState {
    mmap: Arc<Mutex<MmapMut>>,
}

#[repr(C)]
#[derive(Default, Debug, Copy, Clone, Pod, Zeroable)]
struct InternalState {
    latest_queried_task_item_id: u128,
    latest_processed_task_item_id: u128,
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_in_result,
    reason = "irrecoverable error"
)]
impl MemmapState {
    /// Creates a new [`MemmapState`] with the memory-mapped file at the given path.
    ///
    /// # Errors
    /// If the file cannot be created / opened
    ///
    /// # Panics
    /// If the expected state of the [`InternalState`] will be larger than `u64`
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        // Open or create the file with read and write permissions
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        // Ensure the file is at least the size of InternalState
        let default_state = InternalState::default();
        let default_state_bytes = bytemuck::bytes_of(&default_state);
        let expected_len = default_state_bytes
            .len()
            .try_into()
            .expect("the size of default state must fit in a u64");
        if file.metadata()?.len() < expected_len {
            file.set_len(expected_len)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(default_state_bytes)?;
        }

        // Create a mutable memory map of the file
        // SAFETY:
        // we ensured that the size is large enough
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        mmap.flush()?;

        Ok(Self {
            mmap: Arc::new(Mutex::new(mmap)),
        })
    }

    // Generic helper function for getting a TaskItemId
    fn get_task_item_id<F>(&self, field_accessor: F) -> Option<TaskItemId>
    where
        F: Fn(&InternalState) -> u128,
    {
        let mmap = self.mmap.lock().expect("lock should not be poisoned");
        let data = bytemuck::from_bytes::<InternalState>(&mmap[..]);
        let task_item_id = field_accessor(data);
        drop(mmap);

        if task_item_id == 0 {
            None
        } else {
            Some(TaskItemId(uuid::Uuid::from_u128(task_item_id)))
        }
    }

    // Generic helper function for setting a TaskItemId
    fn set_task_item_id<F>(
        &self,
        task_item_id: &TaskItemId,
        field_mutator: F,
    ) -> Result<(), io::Error>
    where
        F: Fn(&mut InternalState, u128),
    {
        let mut mmap = self.mmap.lock().expect("lock should not be poisoned");
        let raw_u128 = task_item_id.0.as_u128();
        let data = bytemuck::from_bytes_mut::<InternalState>(&mut mmap[..]);
        field_mutator(data, raw_u128);
        mmap.flush()?;
        drop(mmap);
        Ok(())
    }
}

impl relayer_amplifier_state::State for MemmapState {
    type Err = io::Error;

    #[tracing::instrument(skip(self), level = "trace", ret)]
    fn latest_queried_task_id(&self) -> Option<TaskItemId> {
        tracing::trace!("getting latest queried task item id");
        self.get_task_item_id(|data| data.latest_queried_task_item_id)
    }

    #[tracing::instrument(skip(self), err)]
    fn set_latest_queried_task_id(&self, task_item_id: TaskItemId) -> Result<(), Self::Err> {
        tracing::info!("updating latest queried task item id");
        self.set_task_item_id(&task_item_id, |data, value| {
            data.latest_queried_task_item_id = value;
        })
    }

    #[tracing::instrument(skip(self), level = "trace", ret)]
    fn latest_processed_task_id(&self) -> Option<TaskItemId> {
        tracing::trace!("getting latest processed task item id");
        self.get_task_item_id(|data| data.latest_processed_task_item_id)
    }

    #[tracing::instrument(skip(self), err)]
    fn set_latest_processed_task_id(&self, task_item_id: TaskItemId) -> Result<(), Self::Err> {
        tracing::info!("updating latest processed task item id");
        self.set_task_item_id(&task_item_id, |data, value| {
            data.latest_processed_task_item_id = value;
        })
    }
}
