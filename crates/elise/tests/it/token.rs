//! The thread's token and its cells: one token a thread at a time, cells
//! read with it shared and written with it exclusive (what doesn't
//! compile is in `tests/ui/`).

use std::sync::Arc;

use elise::{External, GcObject, Heap, HeapOptions, InlinePlatform, LocalToken, TokenCell};

#[test]
fn a_thread_has_one_token_at_a_time() {
    let token = LocalToken::new();
    let again = std::panic::catch_unwind(LocalToken::new);
    assert!(again.is_err(), "a second token while the first lives");
    drop(token);
    // Once it is gone, another; and another thread's is its own.
    let _token = LocalToken::new();
    std::thread::spawn(|| drop(LocalToken::new()))
        .join()
        .unwrap();
}

#[test]
fn a_cell_reads_shared_and_writes_exclusive() {
    let mut token = LocalToken::new();
    let cell = TokenCell::new(1);
    let (a, b) = (cell.get(&token), cell.get(&token));
    assert_eq!((*a, *b), (1, 1));
    *cell.get_mut(&mut token) += 1;
    assert_eq!(cell.replace(&mut token, 5), 2);
    assert_eq!(*cell.get(&token), 5);
    assert_eq!(cell.into_inner(), 5);
}

#[derive(GcObject)]
struct Styled {
    style: External<TokenCell<Option<Arc<u32>>>>,
}

/// A cell in an object (`External`: its value `Send`), read as a
/// reference in an operation, and dropped with it as the heap sweeps it.
#[test]
fn a_cell_in_an_object() {
    let mut token = LocalToken::new();
    // No workers: the collect's sweep is done as it returns.
    let mut heap = Heap::with_platform(HeapOptions::default(), Arc::new(InlinePlatform));
    let style = Arc::new(7);
    let root = heap.mutate(|cx| {
        let styled = cx.alloc(StyledInit {
            style: External::new(TokenCell::new(None)),
        });
        let old = styled.style.get().replace(&mut token, Some(style.clone()));
        assert!(old.is_none());
        assert_eq!(styled.style.get().get(&token).as_deref(), Some(&7));
        elise::Persistent::new(cx, styled)
    });
    assert_eq!(Arc::strong_count(&style), 2);
    heap.read(|cx| {
        let styled = root.get(cx);
        let read: &Option<Arc<u32>> = styled.into_ref().style.get().get(&token);
        assert!(Arc::ptr_eq(read.as_ref().unwrap(), &style), "no clone");
    });
    drop(root);
    heap.collect();
    heap.mutate(|_| {});
    assert_eq!(Arc::strong_count(&style), 1, "dropped with its object");
}
