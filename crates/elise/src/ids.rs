//! The heap's id table: ids for objects that code off the heap refers to
//! (the DOM's style, layout and paint name nodes by id), each to its
//! object while it lives. Ids count up and are never given again, so an id
//! whose object died finds nothing. The table is indexed by id, in chunks:
//! ids given together (a document's nodes, a subtree built at once) are in
//! the same chunks and mostly die together, and a chunk left empty is
//! freed. Blink's `DOMNodeIds` maps ids to nodes with a hash map, but gives
//! ids only to the nodes that need one; here every node has one.

/// An object's id in its heap's table: what `Mutation::new_id` gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(u32);

impl Id {
    pub fn get(self) -> u32 {
        self.0
    }

    /// The id `get` gave, read back from where it was written. Any `raw`
    /// is safe: one never given, or whose object died, finds nothing.
    pub fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
}

/// Ids a chunk holds.
pub(crate) const CHUNK: usize = 1024;

/// The table: a chunk for each `CHUNK` ids (none once all its objects
/// died), each entry an object's offset in the cage (0: no object). Ids
/// to young objects are listed too, for a scavenge to update without
/// walking the table (V8's global handles' young nodes).
#[derive(Default)]
pub(crate) struct IdTable {
    chunks: Vec<Option<Box<[u32; CHUNK]>>>,
    /// The last id given (0: none yet; 0 is never an id).
    last: u32,
    /// The ids whose object is young.
    young: Vec<u32>,
}

impl IdTable {
    /// The next id, for the object at `offset` (young if `young`).
    ///
    /// # Panics
    ///
    /// Once 2^32 - 1 ids were given.
    pub(crate) fn insert(&mut self, offset: u32, young: bool) -> Id {
        self.last = self.last.checked_add(1).expect("fewer than 2^32 ids");
        let id = self.last as usize;
        let (chunk, at) = (id / CHUNK, id % CHUNK);
        if self.chunks.len() <= chunk {
            self.chunks.resize_with(chunk.saturating_add(1), || None);
        }
        self.chunks[chunk].get_or_insert_with(|| Box::new([0; CHUNK]))[at] = offset;
        if young {
            self.young.push(self.last);
        }
        Id(self.last)
    }

    /// The offset of `id`'s object, 0 if none (never given, or died).
    pub(crate) fn get(&self, id: Id) -> u32 {
        let id = id.0 as usize;
        self.chunks
            .get(id / CHUNK)
            .and_then(Option::as_ref)
            .map_or(0, |chunk| chunk[id % CHUNK])
    }

    /// After a full GC's marking: clears the entries `dead` says died, and
    /// frees the chunks left empty.
    pub(crate) fn clear(&mut self, dead: impl Fn(u32) -> bool) {
        for slot in &mut self.chunks {
            if let Some(chunk) = slot {
                for entry in chunk.iter_mut() {
                    if *entry != 0 && dead(*entry) {
                        *entry = 0;
                    }
                }
                if chunk.iter().all(|&entry| entry == 0) {
                    *slot = None;
                }
            }
        }
    }

    /// After a scavenge copied what it keeps: each young id's entry is
    /// what `after` gives (its copy, or 0 if it died), kept in the list
    /// while `young` says the copy is.
    pub(crate) fn update_young(&mut self, after: impl Fn(u32) -> u32, young: impl Fn(u32) -> bool) {
        let mut kept = Vec::new();
        for id in std::mem::take(&mut self.young) {
            let at = id as usize;
            let Some(Some(chunk)) = self.chunks.get_mut(at / CHUNK) else {
                continue;
            };
            let entry = &mut chunk[at % CHUNK];
            *entry = after(*entry);
            if *entry != 0 && young(*entry) {
                kept.push(id);
            }
        }
        self.young = kept;
    }

    /// Each entry: its id and its object's offset (the heap verifier's).
    pub(crate) fn entries(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.chunks.iter().enumerate().flat_map(|(number, chunk)| {
            chunk.iter().flat_map(move |chunk| {
                chunk
                    .iter()
                    .enumerate()
                    .filter(|&(_, &entry)| entry != 0)
                    .map(move |(at, &entry)| {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "an id given, under 2^32"
                        )]
                        let id = number.saturating_mul(CHUNK).saturating_add(at) as u32;
                        (id, entry)
                    })
            })
        })
    }

    /// Whether some chunk is held with no entry (the verifier's: a full GC
    /// frees them).
    pub(crate) fn has_empty_chunk(&self) -> bool {
        self.chunks
            .iter()
            .flatten()
            .any(|chunk| chunk.iter().all(|&entry| entry == 0))
    }

    /// The young ids (the verifier's).
    pub(crate) fn young(&self) -> &[u32] {
        &self.young
    }

    /// Empties `id`'s entry, its chunk kept (the verifier's tests).
    #[cfg(test)]
    pub(crate) fn empty_for_testing(&mut self, id: Id) {
        let id = id.0 as usize;
        if let Some(Some(chunk)) = self.chunks.get_mut(id / CHUNK) {
            chunk[id % CHUNK] = 0;
        }
    }

    /// Chunks held.
    pub(crate) fn chunks(&self) -> usize {
        self.chunks.iter().flatten().count()
    }
}
