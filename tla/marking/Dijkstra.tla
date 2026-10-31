------------------------------ MODULE Dijkstra ------------------------------
(***************************************************************************)
(* Concurrent marking with a Dijkstra (insertion) barrier and black        *)
(* allocation, as V8's and cppgc's, on the same heap as Satb.tla: one      *)
(* mutator that GCs only at safepoints, a root table the marker walks      *)
(* while the mutator runs, weak reads of anything not yet swept.           *)
(*                                                                         *)
(* The barrier marks the value written, if marking has not reached it.     *)
(* Roots get no barrier, neither added nor deleted, and weak reads none:   *)
(* the finishing pause rescans the root table instead (it is a safepoint,  *)
(* so there are no raw pointers to rescan), then drains the worklist and   *)
(* sweeps.                                                                 *)
(*                                                                         *)
(* `Mutation` takes one part of the protocol out; check.py says for each   *)
(* whether TLC should find a counterexample.                               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Obj,        \* the heap's objects (model values)
    NF,         \* fields per object
    Null,       \* an empty field
    None,       \* the marker scanning no object
    Mutation    \* "none", or the part of the protocol taken out

Mutations == {"none", "no_write_barrier", "no_black_allocation",
              "no_root_rescan"}

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
    rescanned   \* the pause has rescanned the root table

vars == <<fresh, alive, edges, roots, locals, phase, marked, grey, cursor,
          pending, rescanned>>

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
    /\ pending \subseteq Obj /\ rescanned \in BOOLEAN
    /\ phase = "idle" => marked = {} /\ pending = {} /\ ~rescanned

\* When the pause has nothing left to do, everything still reachable is
\* marked.
Done ==
    /\ phase = "finishing" /\ rescanned
    /\ grey = {} /\ pending = {} /\ cursor.obj = None
MarkingComplete == Done => Reachable \subseteq marked

\* Its consequence: no reachable object was swept.
NoDangling == Reachable \subseteq alive

\* Violated when the model ever sweeps an object (see Satb.tla).
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
    /\ rescanned = FALSE

(* The mutator, inside an operation. *)

Alloc ==
    /\ Mutating
    /\ \E o \in fresh :
        /\ fresh' = fresh \ {o}
        /\ alive' = alive \cup {o}
        /\ locals' = locals \cup {o}
        /\ marked' = IF phase = "marking" /\ Mutation # "no_black_allocation"
                     THEN marked \cup {o} ELSE marked
    /\ UNCHANGED <<edges, roots, phase, grey, cursor, pending, rescanned>>

LoadRoot ==
    /\ Mutating
    /\ \E r \in roots : locals' = locals \cup {r}
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, marked, grey, cursor,
                   pending, rescanned>>

ReadField ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields :
        /\ edges[o][f] # Null
        /\ locals' = locals \cup {edges[o][f]}
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, marked, grey, cursor,
                   pending, rescanned>>

\* The barrier marks the value written, if marking has not reached it.
Write ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields, v \in locals \cup {Null} :
        /\ edges' = [edges EXCEPT ![o][f] = v]
        /\ IF /\ phase = "marking"
              /\ Mutation # "no_write_barrier"
              /\ v # Null /\ v \notin marked
           THEN /\ marked' = marked \cup {v}
                /\ grey' = grey \cup {v}
           ELSE UNCHANGED <<marked, grey>>
    /\ UNCHANGED <<fresh, alive, roots, locals, phase, cursor, pending,
                   rescanned>>

AddRoot ==
    /\ Mutating
    /\ \E o \in locals : roots' = roots \cup {o}
    /\ UNCHANGED <<fresh, alive, edges, locals, phase, marked, grey, cursor,
                   pending, rescanned>>

DeleteRoot ==
    /\ Mutating
    /\ \E r \in roots : roots' = roots \ {r}
    /\ UNCHANGED <<fresh, alive, edges, locals, phase, marked, grey, cursor,
                   pending, rescanned>>

WeakRead ==
    /\ Mutating
    /\ \E o \in alive : locals' = locals \cup {o}
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, marked, grey, cursor,
                   pending, rescanned>>

\* The operation ends: its raw pointers are gone, and this is a safepoint.
EndOperation ==
    /\ Mutating
    /\ locals # {}
    /\ locals' = {}
    /\ UNCHANGED <<fresh, alive, edges, roots, phase, marked, grey, cursor,
                   pending, rescanned>>

(* The GC. *)

StartMarking ==
    /\ phase = "idle" /\ locals = {}
    /\ phase' = "marking"
    /\ pending' = roots
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, marked, grey, cursor,
                   rescanned>>

\* The marker visits one root-table entry; a deleted one is empty.
ScanRoot ==
    /\ Marking
    /\ \E r \in pending :
        /\ pending' = pending \ {r}
        /\ IF r \in roots /\ r \notin marked
           THEN /\ marked' = marked \cup {r}
                /\ grey' = grey \cup {r}
           ELSE UNCHANGED <<marked, grey>>
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, cursor,
                   rescanned>>

TakeFromWorklist ==
    /\ Marking
    /\ cursor.obj = None
    /\ \E o \in grey :
        /\ grey' = grey \ {o}
        /\ cursor' = [obj |-> o, idx |-> 1]
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, marked,
                   pending, rescanned>>

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
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, pending,
                   rescanned>>

BeginFinish ==
    /\ phase = "marking" /\ locals = {}
    /\ phase' = "finishing"
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, marked, grey, cursor,
                   pending, rescanned>>

\* The pause marks every root (its time grows with the root table).
RescanRoots ==
    /\ phase = "finishing" /\ ~rescanned
    /\ rescanned' = TRUE
    /\ IF Mutation = "no_root_rescan"
       THEN UNCHANGED <<marked, grey>>
       ELSE /\ marked' = marked \cup roots
            /\ grey' = grey \cup (roots \ marked)
    /\ UNCHANGED <<fresh, alive, edges, roots, locals, phase, cursor, pending>>

Sweep ==
    /\ Done
    /\ alive' = alive \cap marked
    /\ edges' = [o \in Obj |-> IF o \in alive \cap marked THEN edges[o]
                               ELSE [f \in Fields |-> Null]]
    /\ marked' = {}
    /\ phase' = "idle"
    /\ rescanned' = FALSE
    /\ UNCHANGED <<fresh, roots, locals, grey, cursor, pending>>

Next ==
    \/ Alloc \/ LoadRoot \/ ReadField \/ Write \/ AddRoot \/ DeleteRoot
    \/ WeakRead \/ EndOperation
    \/ StartMarking \/ ScanRoot \/ TakeFromWorklist \/ ScanField
    \/ BeginFinish \/ RescanRoots \/ Sweep

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Obj)
=============================================================================
