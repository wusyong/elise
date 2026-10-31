//! The pages' bitmap search, proved with Verus for bitmaps of any length:
//! `next_bit` returns the first bit at or after `from`, below `slots`, that
//! is `set`, and `free_ranges` the maximal runs of zeros, lowest first,
//! apart, and every zero in one. A mistake here (a range a slot too long:
//! two objects in one slot) shows to neither Miri nor a sanitizer.
//!
//! What is inside `verus!` is what runs: a plain build erases the specs and
//! proofs and compiles the rest. `python3 scripts/test.py verus` checks
//! the proofs (`cargo verus verify -p elise`).

// The lemmas exist only when Verus verifies: a plain build erases them.
#[cfg(verus_only)]
use vstd::arithmetic::div_mod::lemma_fundamental_div_mod_converse;
use vstd::prelude::*;
#[cfg(verus_only)]
use vstd::std_specs::bits::axiom_u64_trailing_zeros;

verus! {

/// Bit `i` of a bitmap: word `i / 64`'s bit `i % 64`.
pub open spec fn bit(bits: Seq<u64>, i: int) -> bool {
    ((bits[i / 64] >> ((i % 64) as u64)) & 1u64) == 1u64
}

/// Bit `k` of word `word` is the bitmap's bit `word * 64 + k`.
proof fn lemma_index(word: int, k: int)
    requires
        0 <= word,
        0 <= k < 64,
    ensures
        (word * 64 + k) / 64 == word,
        (word * 64 + k) % 64 == k,
{
    lemma_fundamental_div_mod_converse(word * 64 + k, 64, word, k);
}

/// A bit of the candidates: the word's bit (inverted when looking for a
/// clear one) where the mask has it.
proof fn lemma_candidate(w: u64, m: u64, set: bool, k: u64)
    requires
        k < 64,
    ensures
        ((((if set { w } else { !w }) & m) >> k) & 1u64 == 1u64) <==> ((((w >> k) & 1u64) == 1u64) == set
            && ((m >> k) & 1u64) == 1u64),
{
    if set {
        assert(((w & m) >> k) & 1u64 == 1u64 <==> (((w >> k) & 1u64) == 1u64 && ((m >> k) & 1u64) == 1u64))
            by (bit_vector)
            requires
                k < 64,
        ;
    } else {
        assert(((!w & m) >> k) & 1u64 == 1u64 <==> (((w >> k) & 1u64) != 1u64 && ((m >> k) & 1u64) == 1u64))
            by (bit_vector)
            requires
                k < 64,
        ;
    }
}

/// The mask's bit `k`: every bit from `s` up.
proof fn lemma_mask(s: u64, k: u64)
    requires
        s < 64,
        k < 64,
    ensures
        (((0xffff_ffff_ffff_ffffu64 << s) >> k) & 1u64 == 1u64) <==> k >= s,
        ((0xffff_ffff_ffff_ffffu64 >> k) & 1u64) == 1u64,
{
    assert((((0xffff_ffff_ffff_ffffu64 << s) >> k) & 1u64 == 1u64) <==> k >= s) by (bit_vector)
        requires
            s < 64,
            k < 64,
    ;
    assert(((0xffff_ffff_ffff_ffffu64 >> k) & 1u64) == 1u64) by (bit_vector)
        requires
            k < 64,
    ;
}

/// A word with no candidate has none at `k`.
proof fn lemma_zero(c: u64, k: u64)
    requires
        c == 0,
        k < 64,
    ensures
        ((c >> k) & 1u64) == 0u64,
{
    assert(c == 0 ==> ((c >> k) & 1u64) == 0u64) by (bit_vector);
}

/// The first bit at or after `from` and below `slots` that is `set`.
pub(crate) fn next_bit(bits: &[u64], slots: usize, from: usize, set: bool) -> (found: Option<usize>)
    requires
        from <= slots,
        slots <= bits.len() * 64,
        bits.len() <= 0x0100_0000,
    ensures
        match found {
            Some(found) => from <= found < slots && bit(bits@, found as int) == set && forall|i: int|
                from <= i < found ==> bit(bits@, i) != set,
            None => forall|i: int| from <= i < slots ==> bit(bits@, i) != set,
        },
{
    let mut word = from / 64;
    // Bits below `from` in its word are skipped.
    let mut mask = 0xffff_ffff_ffff_ffffu64 << ((from % 64) as u64);
    while word < bits.len()
        invariant
            from <= slots,
            slots <= bits.len() * 64,
            bits.len() <= 0x0100_0000,
            from / 64 <= word <= bits.len(),
            word == from / 64 ==> mask == 0xffff_ffff_ffff_ffffu64 << ((from % 64) as u64),
            word > from / 64 ==> mask == 0xffff_ffff_ffff_ffffu64,
            forall|i: int| from <= i < word * 64 && i < slots ==> bit(bits@, i) != set,
        decreases bits.len() - word,
    {
        let w = bits[word];
        let candidates = if set { w } else { !w } & mask;
        // A bit of this word at or after `from` is `set` exactly where it
        // is a candidate.
        assert forall|k: u64| #![trigger (candidates >> k) & 1u64] k < 64 && from <= word * 64 + k implies (((candidates >> k) & 1u64 == 1u64)
            <==> bit(bits@, word * 64 + k as int) == set) by {
            lemma_candidate(w, mask, set, k);
            lemma_index(word as int, k as int);
            if word == from / 64 {
                lemma_index(word as int, (from % 64) as int);
                lemma_mask((from % 64) as u64, k);
            } else {
                lemma_mask(0, k);
            }
        }
        if candidates != 0 {
            proof {
                axiom_u64_trailing_zeros(candidates);
            }
            let zeros = candidates.trailing_zeros();
            #[expect(
                clippy::arithmetic_side_effects,
                reason = "Verus proves it: under bits.len() * 64, within 2^30"
            )]
            let found = word * 64 + zeros as usize;
            proof {
                let z = zeros as u64;
                assert(z < 64);
                assert((candidates >> z) & 1u64 == 1u64);
                // The found bit is a candidate: past `from`, and `set`.
                lemma_candidate(w, mask, set, z);
                lemma_index(word as int, z as int);
                if word == from / 64 {
                    lemma_index(word as int, (from % 64) as int);
                    lemma_mask((from % 64) as u64, z);
                }
                assert(from <= found);
                assert forall|i: int| from <= i < found && i < slots implies bit(bits@, i) != set by {
                    if i >= word * 64 {
                        let k = (i - word * 64) as u64;
                        assert(k as int == i - word * 64);
                        assert((candidates >> k) & 1u64 == 0u64);
                    }
                }
            }
            return if found < slots {
                Some(found)
            } else {
                None
            };
        }
        proof {
            assert forall|i: int| from <= i < (word + 1) * 64 && i < slots implies bit(bits@, i) != set by {
                if i >= word * 64 {
                    let k = (i - word * 64) as u64;
                    lemma_zero(candidates, k);
                }
            }
        }
        #[expect(clippy::arithmetic_side_effects, reason = "Verus proves it: word < bits.len()")]
        {
            word += 1;
        }
        mask = 0xffff_ffff_ffff_ffffu64;
    }
    None
}

/// A run of zeros among the first `slots` bits: as long as it goes.
pub open spec fn run(bits: Seq<u64>, slots: int, first: int, end: int) -> bool {
    &&& 0 <= first < end <= slots
    &&& forall|i: int| first <= i < end ==> !bit(bits, i)
    &&& first == 0 || bit(bits, first - 1)
    &&& end == slots || bit(bits, end)
}

/// The runs of zeros among the first `slots` bits, `(first, end)`, lowest
/// first; a word at a time.
pub(crate) fn free_ranges(bits: &[u64], slots: usize) -> (ranges: Vec<(u32, u32)>)
    requires
        slots <= bits.len() * 64,
        slots <= 0x4000,
        bits.len() <= 0x0100_0000,
    ensures
        forall|r: int| #![trigger ranges[r]] 0 <= r < ranges.len() ==> run(bits@, slots as int, ranges[r].0 as int, ranges[r].1 as int),
        forall|r: int| #![trigger ranges[r]] 0 <= r < ranges.len() - 1 ==> ranges[r].1 < ranges[r + 1].0,
        forall|i: int| 0 <= i < slots && !bit(bits@, i) ==> exists|r: int|
            #![trigger ranges[r]]
            0 <= r < ranges.len() && ranges[r].0 <= i < ranges[r].1,
{
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut at: usize = 0;
    #[expect(clippy::while_let_loop, reason = "Verus has no `while let`")]
    loop
        invariant
            at <= slots,
            slots <= bits.len() * 64,
            slots <= 0x4000,
            bits.len() <= 0x0100_0000,
            at == 0 || at == slots || bit(bits@, at as int),
            ranges.len() > 0 ==> ranges[ranges.len() - 1].1 == at,
            ranges.len() == 0 ==> at == 0,
            forall|r: int| #![trigger ranges[r]] 0 <= r < ranges.len() ==> run(bits@, slots as int, ranges[r].0 as int, ranges[r].1 as int),
            forall|r: int| #![trigger ranges[r]] 0 <= r < ranges.len() - 1 ==> ranges[r].1 < ranges[r + 1].0,
            forall|i: int| 0 <= i < at && !bit(bits@, i) ==> exists|r: int|
                #![trigger ranges[r]]
                0 <= r < ranges.len() && ranges[r].0 <= i < ranges[r].1,
        ensures
            forall|i: int| 0 <= i < slots && !bit(bits@, i) ==> exists|r: int|
                #![trigger ranges[r]]
                0 <= r < ranges.len() && ranges[r].0 <= i < ranges[r].1,
        decreases slots - at,
    {
        // No zero from `at` on: the runs cover them all.
        let Some(first) = next_bit(bits, slots, at, false) else { break };
        let end = match next_bit(bits, slots, first, true) {
            Some(end) => end,
            None => slots,
        };
        proof {
            // Past the last run: the bit at `at` (if any) is set.
            assert(first > at || at == 0);
            assert(first == 0 || bit(bits@, first - 1));
            assert forall|i: int| 0 <= i < end && !#[trigger] bit(bits@, i) implies (i < at || first <= i) by {}
        }
        let ghost before = ranges@;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "Verus proves it: below slots, at most 2^14"
        )]
        ranges.push((first as u32, end as u32));
        proof {
            assert forall|i: int| 0 <= i < end && !bit(bits@, i) implies exists|r: int|
                #![trigger ranges[r]]
                0 <= r < ranges.len() && ranges[r].0 <= i < ranges[r].1 by {
                if i < at {
                    let r = choose|r: int| #![trigger before[r]] 0 <= r < before.len() && before[r].0 <= i < before[r].1;
                    assert(ranges[r] == before[r]);
                } else {
                    assert(ranges[ranges.len() - 1].0 <= i < ranges[ranges.len() - 1].1);
                }
            }
        }
        at = end;
    }
    ranges
}

} // verus!
