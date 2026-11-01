//! The heap verifier (V8's `--verify-heap`, cppgc's `MarkingVerifier`):
//! after a GC, with `HeapOptions::verify` (or `stress`), it walks the heap
//! again from the roots on its own (a depth-first walk with a set of its
//! own, sharing nothing of the marker's or the scavenger's but the pages'
//! address arithmetic) and checks what the GC left. After a full GC: the
//! nursery is empty, every object reached is whole and marked, nothing
//! else is marked, and the pages' accounts add up. After a scavenge: every
//! object reached is whole, a young one on the nursery's pages and not
//! forwarded, every old object's edge to a young one is in its slot set,
//! and the nursery's accounts add up. It panics with what it found wrong.

use std::collections::HashSet;

use super::{Inner, PageState, large_data};
use crate::page::{self, HEADER_BYTES, PAGE_BYTES, SlotSet};
use crate::types::TypeInfo;
use crate::types::{self, Kind};

impl Inner {
    /// Checks the heap, right after a GC. After a concurrent marking,
    /// `floating` is what the roots reached at its start, what a weak read
    /// reached when it marked its object, and what was allocated in the old
    /// generation since: the only marked objects the roots may no longer
    /// reach (SATB's floating garbage).
    ///
    /// # Panics
    ///
    /// With what is wrong.
    pub(super) fn verify(&self, floating: Option<&HashSet<usize>>) {
        if let Err(problem) = self.check(floating) {
            panic!("heap verifier: {problem}");
        }
    }

    /// Checks the pages' accounts once a sweep that ran alongside the
    /// mutator is done (each page was checked as its result was applied;
    /// allocation has used some since): no page waits for the sweep, every
    /// page a class offers is one of its with free slots, a free page is in
    /// the pool or the discarded, the commits add up, and (in debug builds,
    /// where an object allocated since sets its mark bit) the marked slots
    /// and large objects add up to what the heap uses.
    ///
    /// # Panics
    ///
    /// With what is wrong.
    pub(super) fn verify_space(&self) {
        if let Err(problem) = self.check_space() {
            panic!("heap verifier: {problem}");
        }
    }

    fn check_space(&self) -> Result<(), String> {
        let space = self.space.borrow();
        let mut committed = 0usize;
        for (number, state) in space.states.iter().enumerate() {
            committed = committed.saturating_add(space.committed[number]);
            match state {
                PageState::Small(page) if page.unswept => {
                    return Err(format!("page {number} isn't swept, but the sweep is done"));
                }
                PageState::Free { .. } => {
                    let listed = if space.committed[number] > 0 {
                        &space.pool
                    } else {
                        &space.discarded
                    };
                    if !listed.contains(&number) {
                        return Err(format!(
                            "free page {number} is in neither the pool nor the discarded"
                        ));
                    }
                }
                _ => {}
            }
        }
        if committed != space.committed_total {
            return Err(format!(
                "the pages commit {committed}, not {}",
                space.committed_total
            ));
        }
        for (class, pages) in space.available.iter().enumerate() {
            for &number in pages {
                match &space.states[number] {
                    PageState::Small(page) if page.class == class && page.free > 0 => {}
                    _ => {
                        return Err(format!(
                            "class {class} offers page {number}, which has no free slot of it"
                        ));
                    }
                }
            }
        }
        if cfg!(debug_assertions) {
            let marked = space
                .states
                .iter()
                .map(|state| match state {
                    PageState::Small(page) => page.objects().len().saturating_mul(page.slot_bytes),
                    PageState::Large(object) if object.is_marked() => object.bytes,
                    _ => 0,
                })
                .fold(0usize, usize::saturating_add);
            if marked != self.used.get() {
                return Err(format!(
                    "the slots in use add up to {marked}, not {}, once the sweep is done",
                    self.used.get()
                ));
            }
        }
        Ok(())
    }

    /// The objects the roots reach, by data offset: a walk of the
    /// verifier's own. (A driven heap's engine holds more: the marking
    /// records what it hands over.)
    pub(super) fn reachable(&self) -> HashSet<usize> {
        self.reachable_from(self.shared.roots.in_use())
    }

    /// The objects `stack`'s reach, themselves included, through strong
    /// edges.
    pub(super) fn reachable_from(&self, mut stack: Vec<usize>) -> HashSet<usize> {
        let mut seen = HashSet::new();
        while let Some(data) = stack.pop() {
            if !seen.insert(data) {
                continue;
            }
            // SAFETY: a reachable object's header, in a committed page, and
            // its type.
            let entry = unsafe { types::entry(self.header(data).read()) };
            let ty = &entry.info;
            let mut edge = |at: usize| {
                // SAFETY: an edge of a whole object, by its layout.
                let edge = unsafe { self.ptr(data).add(at).cast::<u32>().read() };
                if edge != 0 {
                    stack.push(edge as usize);
                }
            };
            match ty.kind() {
                Kind::Fixed => {
                    for field in ty.fields() {
                        for i in 0..field.kind.edges() {
                            edge(field.offset.saturating_add(i.saturating_mul(4)));
                        }
                        // Every ephemeron's value, live key or not: what
                        // may float is a superset.
                        for i in 0..field.kind.ephemerons() {
                            edge(
                                field
                                    .offset
                                    .saturating_add(i.saturating_mul(8))
                                    .saturating_add(4),
                            );
                        }
                    }
                }
                Kind::MemberArray => {
                    // SAFETY: an array's capacity.
                    let capacity = unsafe { self.ptr(data).cast::<u32>().read() } as usize;
                    for i in 0..capacity {
                        edge(i.saturating_add(1).saturating_mul(4));
                    }
                }
                // SAFETY: a whole object of the type.
                Kind::Raw => unsafe { entry.raw_edges(self.ptr(data)) }.for_each(edge),
            }
        }
        seen
    }

    /// Checks the heap, right after a scavenge on its own.
    ///
    /// # Panics
    ///
    /// With what is wrong.
    pub(super) fn verify_young(&self) {
        if let Err(problem) = self.check_young() {
            panic!("heap verifier: {problem}");
        }
    }

    fn check(&self, floating: Option<&HashSet<usize>>) -> Result<(), String> {
        // A concurrent marking ends with the mutator's young objects in the
        // nursery: reached, they are walked through, not marked.
        let concurrent = floating.is_some();
        if !concurrent {
            let young = self.young.borrow();
            if !young.pages.is_empty()
                || !young.finalizable.is_empty()
                || self.young_used.get() != 0
            {
                return Err("the nursery isn't empty after a full GC".to_owned());
            }
        }
        let space = self.space.borrow();
        let states = &space.states;
        // What the roots reach: through edges, and an ephemeron's value
        // once its key is reached (young: a scavenge's to judge), again
        // until no more.
        let mut seen = HashSet::new();
        let mut stack: Vec<(usize, String)> = Vec::new();
        for (index, offset) in self.shared.roots.in_use().into_iter().enumerate() {
            stack.push((offset, format!("root {index}")));
        }
        let mut pending: Vec<(u32, u32, String)> = Vec::new();
        loop {
            while let Some((data, from)) = stack.pop() {
                if !seen.insert(data) {
                    continue;
                }
                let young = concurrent && self.is_young(data);
                if young {
                    if !matches!(
                        states.get(page::number_of(data)),
                        Some(PageState::Young { .. })
                    ) {
                        return Err(format!(
                            "{data:#x} is on a young page not the nursery's (reached from {from})"
                        ));
                    }
                } else {
                    check_object(states, data, true)
                        .map_err(|problem| format!("{problem} (reached from {from})"))?;
                }
                // SAFETY: a whole object (just checked), in a committed page.
                let index = unsafe {
                    self.ptr(data)
                        .wrapping_sub(HEADER_BYTES)
                        .cast::<u32>()
                        .read()
                };
                if index == 0 {
                    return Err(format!(
                        "the object at {data:#x} has no type (reached from {from})"
                    ));
                }
                self.check_flags(data, young)
                    .map_err(|problem| format!("{problem} (reached from {from})"))?;
                // SAFETY: a reachable object's type.
                let entry = unsafe { types::entry(index) };
                let ty = &entry.info;
                let mut edge = |at: usize, edge: u32| {
                    if edge != 0 {
                        stack.push((edge as usize, format!("{data:#x}+{at} ({})", ty.name())));
                    }
                };
                match ty.kind() {
                    Kind::Fixed => {
                        for field in ty.fields() {
                            for i in 0..field.kind.edges() {
                                let at = field.offset.saturating_add(i.saturating_mul(4));
                                // SAFETY: a `Member` of a whole object, the
                                // field's `i`th, by its layout.
                                edge(at, unsafe { self.ptr(data).add(at).cast::<u32>().read() });
                            }
                        }
                        for (at, weak) in self.weak_edges(data, ty) {
                            self.check_weak(states, weak, concurrent)
                                .map_err(|problem| {
                                    format!(
                                        "the weak edge at {data:#x}+{at} ({}): {problem}",
                                        ty.name()
                                    )
                                })?;
                        }
                        for (at, key, value) in self.ephemerons(data, ty) {
                            let at = format!("{data:#x}+{at} ({})", ty.name());
                            self.check_pair(states, key, value, concurrent)
                                .map_err(|problem| format!("the ephemeron at {at}: {problem}"))?;
                            if value != 0 {
                                pending.push((key, value, at));
                            }
                        }
                    }
                    Kind::MemberArray => {
                        // SAFETY: an array's capacity, then its edges.
                        let capacity = unsafe { self.ptr(data).cast::<u32>().read() } as usize;
                        for i in 0..capacity {
                            let at = i.saturating_add(1).saturating_mul(4);
                            // SAFETY: an edge of the array.
                            edge(at, unsafe { self.ptr(data).add(at).cast::<u32>().read() });
                        }
                    }
                    Kind::Raw => {
                        // SAFETY: a whole object of the type.
                        for at in unsafe { entry.raw_edges(self.ptr(data)) } {
                            // SAFETY: an edge of the object, by its layout.
                            edge(at, unsafe { self.ptr(data).add(at).cast::<u32>().read() });
                        }
                    }
                }
            }
            let (ready, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|(key, _, _)| {
                seen.contains(&(*key as usize)) || (concurrent && self.is_young(*key as usize))
            });
            pending = rest;
            if ready.is_empty() {
                break;
            }
            stack.extend(ready.into_iter().map(|(_, value, at)| (value as usize, at)));
        }
        for (index, offset) in self.shared.weak_roots.in_use().into_iter().enumerate() {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a root's offset, in the cage, under 2^32"
            )]
            let offset = offset as u32;
            self.check_weak(states, offset, concurrent)
                .map_err(|problem| format!("weak root {index}: {problem}"))?;
        }
        let ids = self.ids.borrow();
        for (id, offset) in ids.entries() {
            self.check_weak(states, offset, concurrent)
                .map_err(|problem| format!("id {id}: {problem}"))?;
        }
        if ids.has_empty_chunk() {
            return Err("the id table holds a chunk with no object".to_owned());
        }
        drop(ids);
        // Each slot set's edge is in a live object: the sweep, which doesn't
        // look at them, freed none that holds one.
        for (number, state) in states.iter().enumerate() {
            for (table, which) in [
                (&self.remembered, "slot set"),
                (&self.remembered_weak, "weak slot set"),
                (&self.remembered_ephemeron, "ephemeron slot set"),
            ] {
                for at in table[number].offsets() {
                    let slot = page::start_of(number).saturating_add(at);
                    let live = match state {
                        PageState::Small(page) => slot
                            .checked_sub(page.start)
                            .and_then(|from| from.checked_div(page.slot_bytes))
                            .is_some_and(|index| index < page.slots && page.is_marked(index)),
                        PageState::Large(object) => object.is_marked(),
                        _ => false,
                    };
                    if !live {
                        return Err(format!(
                            "page {number}'s {which} holds {slot:#x}, which no live object holds"
                        ));
                    }
                }
            }
        }
        // The pages, and what is marked.
        let mut marked = 0usize;
        let mut used = 0usize;
        let mut committed = 0usize;
        for (number, state) in states.iter().enumerate() {
            committed = committed.saturating_add(space.committed[number]);
            match state {
                PageState::Small(page) => {
                    let live = page
                        .check()
                        .map_err(|problem| format!("page {number}: {problem}"))?;
                    if page.unswept
                        && !self.sweeper.holds(number)
                        && !self.main_pages.borrow().contains(&number)
                    {
                        return Err(format!(
                            "page {number} waits for a sweep that doesn't hold it"
                        ));
                    }
                    if page.unswept && page.main_objects > 0 && self.sweeper.holds(number) {
                        return Err(format!(
                            "page {number} has objects to finalize on the heap's thread, but the \
                             sweep's worker holds it"
                        ));
                    }
                    marked = marked.saturating_add(live);
                    used = used.saturating_add(live.saturating_mul(page.slot_bytes));
                    if space.committed[number] < PAGE_BYTES {
                        return Err(format!("page {number} is in use but not all committed"));
                    }
                    if page.free > 0 && !space.available[page.class].contains(&number) {
                        return Err(format!("page {number} has free slots no class offers"));
                    }
                }
                // Dead, its finalizer to run at the next safepoint, or the
                // sweep's worker's.
                PageState::Large(object)
                    if !object.is_marked()
                        && (self.dead_large.borrow().contains(&number)
                            || self.sweeper.holds(number)) => {}
                PageState::Large(object) => {
                    if !object.is_marked() {
                        return Err(format!(
                            "the large object on page {number} is unmarked but kept"
                        ));
                    }
                    marked = marked.saturating_add(1);
                    used = used.saturating_add(object.bytes);
                    let tails =
                        states.get(number.saturating_add(1)..number.saturating_add(object.pages));
                    if !tails.is_some_and(|tails| {
                        tails
                            .iter()
                            .all(|state| matches!(state, PageState::LargeTail))
                    }) {
                        return Err(format!(
                            "the large object on page {number} doesn't own its {} pages",
                            object.pages
                        ));
                    }
                }
                PageState::Free { .. } => {
                    let listed = if space.committed[number] > 0 {
                        &space.pool
                    } else {
                        &space.discarded
                    };
                    if !listed.contains(&number) {
                        return Err(format!(
                            "free page {number} is in neither the pool nor the discarded"
                        ));
                    }
                }
                PageState::Discarding => {
                    if !self.discards.holds(number) || space.committed[number] == 0 {
                        return Err(format!(
                            "page {number} is being discarded, but the job doesn't hold it, \
                             or it holds no memory"
                        ));
                    }
                }
                PageState::Young { .. } if !concurrent => {
                    return Err(format!("page {number} is young after a full GC"));
                }
                PageState::Young { .. } => {}
                PageState::Null | PageState::LargeTail => {}
            }
        }
        match floating {
            None if marked != seen.len() => {
                return Err(format!(
                    "{marked} objects are marked, but the roots reach {}",
                    seen.len()
                ));
            }
            None => {}
            Some(floating) => {
                for (number, state) in states.iter().enumerate() {
                    let marked: Vec<usize> = match state {
                        PageState::Small(page) => page
                            .objects()
                            .into_iter()
                            .map(|slot| page.data_of(slot))
                            .collect(),
                        PageState::Large(object) if object.is_marked() => {
                            vec![large_data(number)]
                        }
                        _ => Vec::new(),
                    };
                    if let Some(data) = marked
                        .into_iter()
                        .find(|data| !seen.contains(data) && !floating.contains(data))
                    {
                        return Err(format!(
                            "the object at {data:#x} is marked, but neither reachable, nor in the \
                             snapshot, nor allocated while marking"
                        ));
                    }
                }
            }
        }
        // While the sweep runs, the heap counts what it used before.
        if !self.sweeping.get() && used != self.used.get() {
            return Err(format!(
                "the slots in use add up to {used}, not {}",
                self.used.get()
            ));
        }
        if committed != space.committed_total {
            return Err(format!(
                "the pages commit {committed}, not {}",
                space.committed_total
            ));
        }
        for (class, pages) in space.available.iter().enumerate() {
            for &number in pages {
                match &states[number] {
                    PageState::Small(page) if page.class == class && page.free > 0 => {}
                    _ => {
                        return Err(format!(
                            "class {class} offers page {number}, which has no free slot of it"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Whether `data` is the start of an object on an old page in use, and
/// marked if `marked`.
fn check_object(states: &[PageState], data: usize, marked: bool) -> Result<(), String> {
    let number = page::number_of(data);
    match states.get(number) {
        Some(PageState::Small(page)) => {
            // A division of its own, not the marker's multiplication.
            let from = data.checked_sub(page.start.saturating_add(HEADER_BYTES));
            let slot = from
                .and_then(|from| from.checked_div(page.slot_bytes))
                .filter(|&slot| slot < page.slots);
            match slot {
                Some(slot) if page.data_of(slot) == data => {
                    if !marked || page.is_marked(slot) {
                        Ok(())
                    } else {
                        Err(format!(
                            "the object at {data:#x} (page {number}, slot {slot}) is reachable but unmarked"
                        ))
                    }
                }
                _ => Err(format!(
                    "{data:#x} isn't the start of a slot of page {number}"
                )),
            }
        }
        Some(PageState::Large(object)) if data == large_data(number) => {
            if !marked || object.is_marked() {
                Ok(())
            } else {
                Err(format!(
                    "the large object at {data:#x} is reachable but unmarked"
                ))
            }
        }
        _ => Err(format!("{data:#x} isn't on a page in use")),
    }
}

impl Inner {
    fn check_young(&self) -> Result<(), String> {
        let space = self.space.borrow();
        let states = &space.states;
        let young = self.young.borrow();
        let cursor = self.young_cursor.get();
        // The nursery's pages, and the end of their objects.
        let mut tops = std::collections::HashMap::new();
        let mut bytes = 0usize;
        for &number in &young.pages {
            let start = page::start_of(number);
            let top = match states.get(number) {
                Some(PageState::Young { .. }) if cursor.page == number => cursor.next,
                Some(PageState::Young { top }) => *top,
                _ => return Err(format!("the nursery's page {number} isn't young")),
            };
            if !self.is_young(start) {
                return Err(format!(
                    "the nursery's page {number} isn't in the young table"
                ));
            }
            tops.insert(number, top);
            bytes = bytes.saturating_add(top.saturating_sub(start));
        }
        // Each page parses from its start to its top: objects (of a
        // registered type) and fillers, none past the top.
        for (&number, &top) in &tops {
            let mut at = page::start_of(number);
            while at < top {
                let object = at.saturating_add(HEADER_BYTES);
                // SAFETY: a header the walk reached, in a young page.
                let index = unsafe { self.header(object).read() };
                if index != super::young::FILLER && !types::is_registered(index) {
                    return Err(format!(
                        "young page {number} has no object or filler at {at:#x}"
                    ));
                }
                at = at.saturating_add(self.object_bytes(object));
            }
            if at != top {
                return Err(format!(
                    "young page {number}'s last object runs past its top"
                ));
            }
        }
        if bytes != self.young_used.get() {
            return Err(format!(
                "the nursery's objects take {bytes} bytes, not {}",
                self.young_used.get()
            ));
        }
        for (number, state) in states.iter().enumerate() {
            if self.is_young(page::start_of(number)) != tops.contains_key(&number) {
                return Err(format!(
                    "page {number} is young in the table but not the nursery's, or the other way ({})",
                    matches!(state, PageState::Young { .. })
                ));
            }
        }
        // Whether `data` is the start of an object of the nursery's (not
        // forwarded): its pages' objects, one after another.
        let young_object = |data: usize| -> Result<&TypeInfo, String> {
            let number = page::number_of(data);
            let top = tops
                .get(&number)
                .ok_or_else(|| format!("{data:#x} is on a young page the nursery doesn't hold"))?;
            let mut at = page::start_of(number);
            while at < *top {
                let object = at.saturating_add(HEADER_BYTES);
                // SAFETY: an object's or filler's header, in a young page.
                if unsafe { self.header(object).read() } == super::young::FILLER {
                    if object == data {
                        return Err(format!("{data:#x} is a filler"));
                    }
                } else if object == data {
                    // SAFETY: the header of an object of the nursery's.
                    let (index, flags) =
                        unsafe { (self.header(data).read(), self.header(data).add(1).read()) };
                    if flags & !(super::young::AGED | super::young::SITE_BITS) != 0 {
                        return Err(format!("the young object at {data:#x} is forwarded"));
                    }
                    if index == 0 {
                        return Err(format!("the young object at {data:#x} has no type"));
                    }
                    // SAFETY: an object of the nursery's type.
                    return Ok(unsafe { types::info(index) });
                }
                at = at.saturating_add(self.object_bytes(object));
            }
            Err(format!(
                "{data:#x} isn't the start of an object of its young page"
            ))
        };
        for &data in &young.finalizable {
            let info = young_object(data).map_err(|problem| format!("{problem} (finalizable)"))?;
            if info.drop_fn().is_none() {
                return Err(format!(
                    "the young object at {data:#x} is listed as finalizable without drop glue"
                ));
            }
        }
        // What the roots reach. (An engine's wrappers hold only old objects,
        // which a scavenge doesn't judge.)
        let mut seen = HashSet::new();
        let mut stack: Vec<(usize, String)> = Vec::new();
        for (index, offset) in self.shared.roots.in_use().into_iter().enumerate() {
            stack.push((offset, format!("root {index}")));
        }
        while let Some((data, from)) = stack.pop() {
            if !seen.insert(data) {
                continue;
            }
            let reached = |problem: String| format!("{problem} (reached from {from})");
            let young_host = self.is_young(data);
            if young_host {
                young_object(data).map_err(reached)?;
            } else {
                check_object(states, data, false).map_err(reached)?;
            }
            self.check_flags(data, young_host).map_err(reached)?;
            // SAFETY: a whole object (just checked), in a committed page.
            let index = unsafe { self.header(data).read() };
            // SAFETY: a reachable object's type.
            let entry = unsafe { types::entry(index) };
            let ty = &entry.info;
            let edges: Vec<usize> = match ty.kind() {
                Kind::Fixed => ty
                    .fields()
                    .iter()
                    .flat_map(|field| {
                        (0..field.kind.edges())
                            .map(|i| field.offset.saturating_add(i.saturating_mul(4)))
                    })
                    .collect(),
                Kind::MemberArray => {
                    // SAFETY: an array's capacity.
                    let capacity = unsafe { self.ptr(data).cast::<u32>().read() } as usize;
                    (0..capacity)
                        .map(|i| i.saturating_add(1).saturating_mul(4))
                        .collect()
                }
                // SAFETY: a whole object of the type.
                Kind::Raw => unsafe { entry.raw_edges(self.ptr(data)) }.collect(),
            };
            for at in edges {
                let slot = data.saturating_add(at);
                // SAFETY: an edge of a whole object.
                let edge = unsafe { self.ptr(slot).cast::<u32>().read() } as usize;
                if edge == 0 {
                    continue;
                }
                if !young_host && self.is_young(edge) && !self.remembered_at(states, slot) {
                    return Err(format!(
                        "the old edge at {data:#x}+{at} ({}) to the young object at {edge:#x} isn't in a slot set",
                        ty.name()
                    ));
                }
                stack.push((edge, format!("{data:#x}+{at} ({})", ty.name())));
            }
            for (at, key, value) in self.ephemerons(data, ty) {
                let slot = data.saturating_add(at);
                let reached = |problem: String| {
                    format!("the ephemeron at {data:#x}+{at} ({}): {problem}", ty.name())
                };
                if key == 0 {
                    if value != 0 {
                        return Err(reached("a value without a key".to_owned()));
                    }
                    continue;
                }
                let key = key as usize;
                if self.is_young(key) {
                    young_object(key).map_err(reached)?;
                    let remembered = self.remembered_in(&self.remembered_ephemeron, states, slot);
                    if !young_host && !remembered {
                        return Err(reached(
                            "its key, old to young, isn't in an ephemeron slot set".to_owned(),
                        ));
                    }
                } else {
                    check_object(states, key, false).map_err(reached)?;
                }
                // Its value is an edge to a scavenge.
                let value_slot = slot.saturating_add(4);
                let value = value as usize;
                if value != 0 {
                    if !young_host
                        && self.is_young(value)
                        && !self.remembered_at(states, value_slot)
                    {
                        return Err(reached(
                            "its value, old to young, isn't in a slot set".to_owned(),
                        ));
                    }
                    stack.push((value, reached("value".to_owned())));
                }
            }
            for (at, weak) in self.weak_edges(data, ty) {
                let slot = data.saturating_add(at);
                let reached = |problem: String| {
                    format!("the weak edge at {data:#x}+{at} ({}): {problem}", ty.name())
                };
                if weak == 0 {
                    continue;
                }
                let weak = weak as usize;
                if self.is_young(weak) {
                    young_object(weak).map_err(reached)?;
                    let remembered = self.remembered_in(&self.remembered_weak, states, slot);
                    if !young_host && !remembered {
                        return Err(reached(
                            "old to young, but not in a weak slot set".to_owned(),
                        ));
                    }
                } else {
                    check_object(states, weak, false).map_err(reached)?;
                }
            }
        }
        for (index, offset) in self.shared.weak_roots.in_use().into_iter().enumerate() {
            let reached = |problem: String| format!("weak root {index}: {problem}");
            if self.is_young(offset) {
                young_object(offset).map_err(reached)?;
            } else {
                check_object(states, offset, false).map_err(reached)?;
            }
        }
        // The id table: every entry a young object's of the nursery (and
        // its id listed as young) or an old object's.
        let ids = self.ids.borrow();
        let listed: HashSet<u32> = ids.young().iter().copied().collect();
        for (id, offset) in ids.entries() {
            let offset = offset as usize;
            let reached = |problem: String| format!("id {id}: {problem}");
            if self.is_young(offset) {
                young_object(offset).map_err(reached)?;
                if !listed.contains(&id) {
                    return Err(reached("a young object's, not listed as young".to_owned()));
                }
            } else {
                check_object(states, offset, false).map_err(reached)?;
            }
        }
        for &id in ids.young() {
            let offset = ids.get(crate::Id::from_raw(id)) as usize;
            if offset == 0 || !self.is_young(offset) {
                return Err(format!("id {id} is listed as young, but its object isn't"));
            }
        }
        Ok(())
    }
}

impl Inner {
    /// The weak edges of the whole object at `data`, of type `ty`, by its
    /// layout: each one's offset and value.
    fn weak_edges(&self, data: usize, ty: &TypeInfo) -> Vec<(usize, u32)> {
        if ty.kind() != Kind::Fixed {
            return Vec::new();
        }
        ty.fields()
            .iter()
            .flat_map(|field| {
                (0..field.kind.weak_edges())
                    .map(|i| field.offset.saturating_add(i.saturating_mul(4)))
            })
            // SAFETY: a `WeakMember` of a whole object, by its layout.
            .map(|at| (at, unsafe { self.ptr(data).add(at).cast::<u32>().read() }))
            .collect()
    }

    /// The ephemerons of the whole object at `data`, of type `ty`, by its
    /// layout: each one's offset, key and value.
    fn ephemerons(&self, data: usize, ty: &TypeInfo) -> Vec<(usize, u32, u32)> {
        if ty.kind() != Kind::Fixed {
            return Vec::new();
        }
        ty.fields()
            .iter()
            .flat_map(|field| {
                (0..field.kind.ephemerons())
                    .map(|i| field.offset.saturating_add(i.saturating_mul(8)))
            })
            .map(|at| {
                // SAFETY: an `EphemeronPair` of a whole object, by its
                // layout: its key, then its value.
                unsafe {
                    let pair = self.ptr(data).add(at).cast::<u32>();
                    (at, pair.read(), pair.add(1).read())
                }
            })
            .collect()
    }

    /// After a full GC, an ephemeron: both none (cleared), or a key as a
    /// weak edge's (`check_weak`) and a value marked (or young, after a
    /// concurrent marking).
    fn check_pair(
        &self,
        states: &[PageState],
        key: u32,
        value: u32,
        concurrent: bool,
    ) -> Result<(), String> {
        if key == 0 {
            return if value == 0 {
                Ok(())
            } else {
                Err("a value without a key".to_owned())
            };
        }
        self.check_weak(states, key, concurrent)?;
        let value = value as usize;
        if value == 0 || (concurrent && self.is_young(value)) {
            return Ok(());
        }
        check_object(states, value, true)
            .map_err(|problem| format!("{problem}: a live key's value, not marked"))
    }

    /// After a full GC, a weak edge's (or root's) value: none, a marked old
    /// object (one not marked was cleared), or, after a concurrent marking,
    /// a young object (a scavenge's to judge).
    fn check_weak(&self, states: &[PageState], weak: u32, concurrent: bool) -> Result<(), String> {
        let weak = weak as usize;
        if weak == 0 {
            return Ok(());
        }
        if concurrent && self.is_young(weak) {
            return match states.get(page::number_of(weak)) {
                Some(PageState::Young { .. }) => Ok(()),
                _ => Err(format!("{weak:#x} is on a young page not the nursery's")),
            };
        }
        check_object(states, weak, true)
            .map_err(|problem| format!("{problem}: a weak edge to a dead object, not cleared"))
    }

    /// Whether the flags of the whole object at `data` are what they may
    /// be: a site the heap allocated from, and `AGED` if it is young.
    fn check_flags(&self, data: usize, young: bool) -> Result<(), String> {
        // SAFETY: a whole object's header, in a committed page.
        let flags = unsafe { self.header(data).add(1).read() };
        let allowed = if young {
            super::young::AGED | super::young::SITE_BITS
        } else {
            super::young::SITE_BITS
        };
        if flags & !allowed != 0 {
            return Err(format!("the object at {data:#x} has flags {flags:#x}"));
        }
        let site = (flags >> crate::site::SITE_SHIFT) as usize;
        if site != 0 && site >= self.sites.borrow().len() {
            return Err(format!(
                "the object at {data:#x} is of site {site}, which the heap never allocated from"
            ));
        }
        Ok(())
    }

    /// Whether the old edge at `slot` is in its page's (or large object's)
    /// slot set.
    fn remembered_at(&self, states: &[PageState], slot: usize) -> bool {
        self.remembered_in(&self.remembered, states, slot)
    }

    /// Whether the old edge at `slot` is in `table`'s set for its page (or
    /// large object).
    fn remembered_in(&self, table: &[SlotSet], states: &[PageState], slot: usize) -> bool {
        let mut number = page::number_of(slot);
        while matches!(states.get(number), Some(PageState::LargeTail)) {
            number = number.saturating_sub(1);
        }
        table[number].contains(slot.saturating_sub(page::start_of(number)))
    }
}
