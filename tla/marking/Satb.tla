-------------------------------- MODULE Satb --------------------------------
(***************************************************************************)
(* Concurrent marking with a snapshot-at-the-beginning (Yuasa, deletion)   *)
(* barrier and black allocation, on a heap with one mutator that GCs only  *)
(* at safepoints: the protocol elise means to use.                         *)
(*                                                                         *)
(* The mutator works in operations. Inside one it holds raw pointers       *)
(* (`locals`, Rust's Gc<'gc, T>) and may allocate, read and write fields,  *)
(* add and delete roots (the root table's Persistents) and read weak       *)
(* references (the id table: any object not yet swept, garbage included).  *)
(* Between operations is a safepoint: no locals, and only there does       *)
(* marking start or finish.                                                *)
(*                                                                         *)
(* Marking starts without scanning anything: the marker walks the root     *)
(* table itself, one entry at a time, while the mutator runs. So deleting  *)
(* a root needs the barrier as overwriting a field does. A weak read may   *)
(* hand out an object the snapshot no longer reaches, so it marks it.      *)
(* Objects allocated while marking are marked (black allocation). The      *)
(* finishing pause drains the barrier's buffer and the worklist, then      *)
(* sweeps what is not marked; it rescans nothing.                          *)
(*                                                                         *)
(* A write is one step: the mutator never sees the phase change halfway    *)
(* through one, as the phase changes only at safepoints. The marker scans  *)
(* an object one field per step, so writes interleave with its reads.      *)
(*                                                                         *)
(* `Mutation` takes one part of the protocol out; check.py expects TLC to  *)
(* find a counterexample for each.                                         *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Obj,        \* the heap's objects (model values)
    NF,         \* fields per object
    Null,       \* an empty field
    None,       \* the marker scanning no object
    Mutation    \* "none", or the part of the protocol taken out

Mutations == {"none", "no_write_barrier", "no_black_allocation",
              "no_root_delete_barrier", "no_weak_read_barrier"}

ASSUME Mutation \in Mutations
ASSUME NF \in Nat \ {0}
ASSUME Null \notin Obj /\ None \notin Obj

Fields == 1..NF
NoCursor == [obj |-> None, idx |-> 0]

VARIABLES
    fresh,      \* never allocated
    alive,      \* allocated and not swept
    edges,      \* each object's fields
    roots,      \* the root table
    locals,     \* the current operation's raw pointers
    phase,      \* "idle", "marking" or "finishing" (the pause)
    marked,     \* mark bits
    grey,       \* the marker's worklist
    cursor,     \* the object the marker is scanning and its next field
    pending,    \* root-table entries the marker has yet to visit
    buffer      \* the barrier's buffer of overwritten values

vars == <<fresh, alive, edges, roots, locals, phase, marked, grey, cursor,
          pending, buffer>>

Mutating == phase \in {"idle", "marking"}
Marking == phase \in {"marking", "finishing"}

RECURSIVE Closure(_)
Closure(S) ==
    LET next == S \cup ({edges[o][f] : o \in S, f \in Fields} \ {Null})
    IN IF next = S THEN S ELSE Closure(next)

\* What the mutator can still get to without a weak read.
Reachable == Closure(roots \cup locals)

TypeOK ==
    /\ fresh \subseteq Obj /\ alive \subseteq Obj /\ fresh \cap alive = {}
    /\ edges \in [Obj -> [Fields -> Obj \cup {Null}]]
    /\ roots \subseteq Obj /\ locals \subseteq alive
    /\ phase \in {"idle", "marking", "finishing"}
    /\ marked \subseteq Obj /\ grey \subseteq marked
    /\ cursor \in [obj : Obj \cup {None}, idx : 0..NF]
    /\ pending \subseteq Obj /\ buffer \subseteq Obj
    /\ phase = "idle" => marked = {} /\ pending = {} /\ buffer = {}

\* The invariant the barrier exists for: when the pause has nothing left
\* to do, everything still reachable is marked.
Done ==
    /\ phase = "finishing"
    /\ grey = {} /\ buffer = {} /\ pending = {} /\ cursor.obj = None
MarkingComplete == Done => Reachable \subseteq marked

\* Its consequence: no reachable object was swept.
NoDangling == Reachable \subseteq alive

\* Violated when the model ever sweeps an object: check.py expects TLC to
\* find that, so the checks above are about a heap that collects.
NothingSwept == fresh \cup alive = Obj

Init ==
    /\ fresh = Obj
    /\ alive = {}
    /\ edges = [o \in Obj |-> [f \in Fields |-> Null]]
    /\ roots = {}
    /\ locals = {}
    /\ phase = "idle"
    /\ marked = {}
    /\ grey = {}
    /\ cursor = NoCursor
    /\ pending = {}
    /\ buffer = {}

(* The mutator, inside an operation. *)

Alloc ==
    /\ Mutating
    /\ \E o \in fresh :
        /\ fresh' = fresh \ {o}
        /\ alive' = alive \cup {o}
        /\ locals' = locals \cup {o}
        /\ marked' = IF phase = "marking" /\ Mutation # "no_black_allocation"
                     THEN marked \cup {o} ELSE marked
    /\ UNCHANGED <<edges, roots, phase, grey, cursor, pending, buffer>>

LoadRoot ==
    /\ Mutating
    /\ \E r \in roots : locals' = locals \cup {r}
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, marked, grey, cursor,
                   pending, buffer>>

ReadField ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields :
        /\ edges[o][f] # Null
        /\ locals' = locals \cup {edges[o][f]}
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, marked, grey, cursor,
                   pending, buffer>>

\* The barrier records the value overwritten, if marking has not reached it.
Write ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields, v \in locals \cup {Null} :
        LET old == edges[o][f] IN
        /\ edges' = [edges EXCEPT ![o][f] = v]
        /\ buffer' = IF /\ phase = "marking"
                        /\ Mutation # "no_write_barrier"
                        /\ old # Null /\ old \notin marked
                     THEN buffer \cup {old} ELSE buffer
    /\ UNCHANGED <<fresh, alive, roots, locals, phase, marked, grey, cursor,
                   pending>>

\* A new root's object came from the snapshot, an allocation or a weak read
\* (which marks it), so adding one needs no barrier.
AddRoot ==
    /\ Mutating
    /\ \E o \in locals : roots' = roots \cup {o}
    /\ UNCHANGED <<fresh, alive, edges, locals, phase, marked, grey, cursor,
                   pending, buffer>>

\* Deleting a root overwrites a root-table entry: the same barrier.
DeleteRoot ==
    /\ Mutating
    /\ \E r \in roots :
        /\ roots' = roots \ {r}
        /\ buffer' = IF /\ phase = "marking"
                        /\ Mutation # "no_root_delete_barrier"
                        /\ r \notin marked
                     THEN buffer \cup {r} ELSE buffer
    /\ UNCHANGED <<fresh, alive, edges, locals, phase, marked, grey, cursor,
                   pending>>

\* A weak read may return garbage marking has not swept yet: marking it
\* keeps it (the keep-alive barrier), as the snapshot doesn't.
WeakRead ==
    /\ Mutating
    /\ \E o \in alive :
        /\ locals' = locals \cup {o}
        /\ IF phase = "marking" /\ Mutation # "no_weak_read_barrier"
                 /\ o \notin marked
           THEN /\ marked' = marked \cup {o}
                /\ grey' = grey \cup {o}
           ELSE UNCHANGED <<marked, grey>>
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, cursor, pending, buffer>>

\* The operation ends: its raw pointers are gone, and this is a safepoint.
EndOperation ==
    /\ Mutating
    /\ locals # {}
    /\ locals' = {}
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, marked, grey, cursor,
                   pending, buffer>>

(* The GC. *)

StartMarking ==
    /\ phase = "idle" /\ locals = {}
    /\ phase' = "marking"
    /\ pending' = roots
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, marked, grey, cursor,
                   buffer>>

\* The marker visits one root-table entry; a deleted one is empty.
ScanRoot ==
    /\ Marking
    /\ \E r \in pending :
        /\ pending' = pending \ {r}
        /\ IF r \in roots /\ r \notin marked
           THEN /\ marked' = marked \cup {r}
                /\ grey' = grey \cup {r}
           ELSE UNCHANGED <<marked, grey>>
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, cursor, buffer>>

TakeFromWorklist ==
    /\ Marking
    /\ cursor.obj = None
    /\ \E o \in grey :
        /\ grey' = grey \ {o}
        /\ cursor' = [obj |-> o, idx |-> 1]
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, marked,
                   pending, buffer>>

ScanField ==
    /\ Marking
    /\ cursor.obj # None
    /\ LET t == edges[cursor.obj][cursor.idx] IN
        /\ IF t # Null /\ t \notin marked
           THEN /\ marked' = marked \cup {t}
                /\ grey' = grey \cup {t}
           ELSE UNCHANGED <<marked, grey>>
        /\ cursor' = IF cursor.idx = NF THEN NoCursor
                     ELSE [cursor EXCEPT !.idx = @ + 1]
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, pending, buffer>>

\* The mutator hands its buffer over (at any time: when it does doesn't
\* matter to safety).
DrainBuffer ==
    /\ Marking
    /\ buffer # {}
    /\ marked' = marked \cup buffer
    /\ grey' = grey \cup (buffer \ marked)
    /\ buffer' = {}
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, cursor, pending>>

BeginFinish ==
    /\ phase = "marking" /\ locals = {}
    /\ phase' = "finishing"
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, marked, grey, cursor,
                   pending, buffer>>

Sweep ==
    /\ Done
    /\ alive' = alive \cap marked
    /\ edges' = [o \in Obj |-> IF o \in alive \cap marked THEN edges[o]
                               ELSE [f \in Fields |-> Null]]
    /\ marked' = {}
    /\ phase' = "idle"
    /\ UNCHANGED <<fresh, roots, locals, grey, cursor, pending, buffer>>

Next ==
    \/ Alloc \/ LoadRoot \/ ReadField \/ Write \/ AddRoot \/ DeleteRoot
    \/ WeakRead \/ EndOperation
    \/ StartMarking \/ ScanRoot \/ TakeFromWorklist \/ ScanField
    \/ DrainBuffer \/ BeginFinish \/ Sweep

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Obj)
=============================================================================
