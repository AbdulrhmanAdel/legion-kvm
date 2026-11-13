//! A guest's video memory, counted and held to `--vram-limit-mib`.

use std::collections::HashMap;

/// An RM client handle (`hClient` / `hRoot`).
pub type Client = u32;
/// An RM object handle, unique within its client.
pub type Handle = u32;

pub struct Vram {
    limit: Option<u64>,
    objects: HashMap<(Client, Handle), Object>,
    memory: HashMap<u64, Memory>,
    next_memory: u64,
    /// Which clients were made on which open file, so closing the file releases them as RM does.
    files: HashMap<u64, Vec<Client>>,
    in_use: u64,
    peak: u64,
    refused: u64,
}

struct Object {
    parent: Handle,
    memory: Option<u64>,
}

struct Memory {
    bytes: u64,
    /// Handles that hold it: the one it was made under and every duplicate.
    holders: u32,
}

/// One MiB, the unit the limit is given in.
const MIB: u64 = 1 << 20;

impl Vram {
    /// `None` counts without limiting.
    pub fn new(limit_mib: Option<u64>) -> Self {
        Self {
            limit: limit_mib.map(|m| m.saturating_mul(MIB)),
            objects: HashMap::new(),
            memory: HashMap::new(),
            next_memory: 1,
            files: HashMap::new(),
            in_use: 0,
            peak: 0,
            refused: 0,
        }
    }

    /// The limit, in bytes.
    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// The limit as the device config announces it: MiB, 0 for none.
    pub fn limit_mib(&self) -> u64 {
        self.limit.map_or(0, |b| b / MIB)
    }

    /// Video memory the guest holds now, in bytes.
    pub fn in_use(&self) -> u64 {
        self.in_use
    }

    /// The most it has held at once.
    pub fn peak(&self) -> u64 {
        self.peak
    }

    /// Allocations refused for want of room.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// What is left under the limit; `None` without one.
    pub fn remaining(&self) -> Option<u64> {
        self.limit.map(|l| l.saturating_sub(self.in_use))
    }

    /// Whether an allocation of `bytes` fits.
    pub fn admit(&mut self, bytes: u64) -> bool {
        let fits = match self.limit {
            None => true,
            Some(l) => self.in_use.checked_add(bytes).is_some_and(|t| t <= l),
        };
        if !fits {
            self.refused += 1;
        }
        fits
    }

    /// The host made `handle` under `parent` in `client`, holding `bytes` of video memory (`None` for an object that holds none).
    pub fn allocated(
        &mut self,
        client: Client,
        parent: Handle,
        handle: Handle,
        bytes: Option<u64>,
    ) {
        // RM does not hand out a live handle twice.
        self.drop_object(client, handle);
        let memory = bytes.map(|b| {
            let id = self.next_memory;
            self.next_memory += 1;
            self.memory.insert(
                id,
                Memory {
                    bytes: b,
                    holders: 1,
                },
            );
            self.in_use = self.in_use.saturating_add(b);
            self.peak = self.peak.max(self.in_use);
            id
        });
        self.objects
            .insert((client, handle), Object { parent, memory });
    }

    /// The host made `handle` in `client` a duplicate of `src_handle` in `src_client`.
    pub fn duplicated(
        &mut self,
        client: Client,
        parent: Handle,
        handle: Handle,
        src_client: Client,
        src_handle: Handle,
    ) {
        let memory = self
            .objects
            .get(&(src_client, src_handle))
            .and_then(|o| o.memory);
        // Held before the old record at `handle` is dropped: if that record was the source itself, dropping it first would free the memory the duplicate is about to hold.
        if let Some(m) = memory.and_then(|id| self.memory.get_mut(&id)) {
            m.holders += 1;
        }
        self.drop_object(client, handle);
        self.objects
            .insert((client, handle), Object { parent, memory });
    }

    /// The host freed `handle` in `client`, and with it everything under it.
    pub fn freed(&mut self, client: Client, handle: Handle) {
        if handle == client {
            let all: Vec<Handle> = self
                .objects
                .keys()
                .filter(|(c, _)| *c == client)
                .map(|(_, h)| *h)
                .collect();
            for h in all {
                self.drop_object(client, h);
            }
            return;
        }
        let mut children: HashMap<Handle, Vec<Handle>> = HashMap::new();
        for ((c, h), o) in &self.objects {
            if *c == client && *h != o.parent {
                children.entry(o.parent).or_default().push(*h);
            }
        }
        let mut doomed = vec![handle];
        let mut i = 0;
        while i < doomed.len() {
            if let Some(kids) = children.remove(&doomed[i]) {
                doomed.extend(kids);
            }
            i += 1;
        }
        for h in doomed {
            self.drop_object(client, h);
        }
    }

    /// `client` was made on the open file `file`.
    pub fn client_opened(&mut self, file: u64, client: Client) {
        let v = self.files.entry(file).or_default();
        if !v.contains(&client) {
            v.push(client);
        }
    }

    /// Whether `client` was made on the open file `file` and is still alive.
    pub fn issued(&self, file: u64, client: Client) -> bool {
        self.files.get(&file).is_some_and(|v| v.contains(&client))
    }

    /// `client` itself was freed, on whichever file it was made on.
    pub fn client_freed(&mut self, client: Client) {
        for v in self.files.values_mut() {
            v.retain(|&c| c != client);
        }
        self.freed(client, client);
    }

    /// The open file `file` was closed: RM frees every client made on it.
    pub fn file_closed(&mut self, file: u64) {
        for client in self.files.remove(&file).unwrap_or_default() {
            self.freed(client, client);
        }
    }

    fn drop_object(&mut self, client: Client, handle: Handle) {
        let Some(o) = self.objects.remove(&(client, handle)) else {
            return;
        };
        let Some(id) = o.memory else { return };
        let Some(m) = self.memory.get_mut(&id) else {
            return;
        };
        m.holders -= 1;
        if m.holders == 0 {
            self.in_use = self.in_use.saturating_sub(m.bytes);
            self.memory.remove(&id);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/vram.rs"]
mod tests;
