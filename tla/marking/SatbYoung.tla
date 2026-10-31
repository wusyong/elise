------------------------------ MODULE SatbYoung ------------------------------
(***************************************************************************)
(* Concurrent marking of the old generation alongside the mutator, with a  *)
(* young generation that scavenges: the protocol elise implements          *)
(* (step 4a). Satb.tla is the protocol of step 1a, without generations;    *)
(* this one is the implementation's:                                       *)
(*                                                                         *)
(* - A marking starts at a safepoint, right after a scavenge that promotes *)
(*   every survivor (the nursery is empty), with every root marked.        *)
(* - The marker scans only old objects: it skips an edge to a young one.   *)
(* - The barrier (snapshot at the beginning) records a field's or a root's *)
(*   value overwritten, if it is old and not marked; young ones it skips.  *)
(* - What enters the old generation while marking is marked: allocated     *)
(*   there (black allocation), or promoted by a scavenge (which runs at a  *)
(*   safepoint, the marker paused).                                        *)
(* - The finishing pause drains the barrier's buffer and the worklist,     *)
(*   then sweeps the old generation's unmarked; the nursery stays.         *)
(*                                                                         *)
(* - An ephemeron: a weak edge to its key, and a value alive while the    *)
(*   key is (no value: a plain weak edge); one in the heap is enough for   *)
(*   each counterexample (one per object made the model too large to       *)
(*   check). Read while marking runs, its key is marked (the weak read     *)
(*   barrier); writing it is a strong edge's write for its old value (the  *)
(*   SATB barrier). The marker, taking its holder, marks the value if the  *)
(*   key is old and marked, or young; else records (discovers) it, and     *)
(*   marks the value once the key is marked (the end waits for that). The  *)
(*   end clears it, key and value, if recorded and its key still isn't     *)
(*   marked; a scavenge keeps the value as an edge's, and clears the pair  *)
(*   if it frees the key. A black holder is never recorded: what it holds  *)
(*   was written while marking ran, so is marked by the end. Weak roots    *)
(*   aren't modelled: the end looks at all of them, recorded or not.       *)
(*                                                                         *)
(* A scavenge keeps the young objects the roots and every old object (the  *)
(* slot sets, the unswept dead's too) reach through any objects, and       *)
(* frees the rest; of those it keeps, it promotes any. Addresses are left  *)
(* out: an object keeps its identity when it moves.                        *)
(*                                                                         *)
(* `Mutation` takes one part out; check.py expects a counterexample for    *)
(* each, but the roots' barrier, which this protocol makes needless: the   *)
(* roots are all marked at the start, and a root added since holds what   *)
(* the snapshot, an allocation or a weak read gave it.                     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Obj,        \* the heap's objects (model values)
    NF,         \* fields per object
    Null,       \* an empty field
    None,       \* the marker scanning no object
    Mutation    \* "none", or the part of the protocol taken out

Mutations == {"none", "no_write_barrier", "no_black_allocation",
              "no_black_promotion", "no_scavenge_at_start",
              "no_root_delete_barrier", "no_weak_read_barrier",
              "no_weak_clear", "no_weak_scavenge_clear", "no_pair_fixpoint",
              "no_pair_value_barrier"}

ASSUME Mutation \in Mutations
ASSUME NF \in Nat \ {0}
ASSUME Null \notin Obj /\ None \notin Obj

Fields == 1..NF
NoCursor == [obj |-> None, idx |-> 0]

VARIABLES
    fresh,      \* never allocated
    alive,      \* allocated and not freed
    young,      \* the nursery's (alive)
    edges,      \* each object's fields
    roots,      \* the root table
    locals,     \* the current operation's raw pointers
    phase,      \* "idle", "marking" or "finishing" (the pause)
    marked,     \* mark bits (old objects')
    grey,       \* the marker's worklist
    cursor,     \* the object the marker is scanning and its next field
    buffer,     \* the barrier's buffer of overwritten values
    holder,     \* the object holding the ephemeron, or Null for none
    target,     \* its key (Null if no holder)
    value,      \* its value, or Null for none (a weak edge)
    recorded    \* the marker recorded (discovered) it

vars == <<fresh, alive, young, edges, roots, locals, phase, marked, grey,
          cursor, buffer, holder, target, value, recorded>>
weakVars == <<holder, target, value, recorded>>

Mutating == phase \in {"idle", "marking"}
Marking == phase \in {"marking", "finishing"}
Old == alive \ young

RECURSIVE Closure(_)
Closure(S) ==
    LET next == S \cup ({edges[o][f] : o \in S, f \in Fields} \ {Null})
    IN IF next = S THEN S ELSE Closure(next)

\* The same, the ephemeron's value included once its holder and its key
\* are: what the mutator can get to (it reads the value through its key).
RECURSIVE EClosure(_)
EClosure(S) ==
    LET reached == Closure(S)
        next == IF holder \in reached /\ target \in reached /\ value # Null
                THEN reached \cup {value} ELSE reached
    IN IF next = S THEN S ELSE EClosure(next)

\* What a scavenge keeps: the value as an edge's, whether its key lives.
SClosure(S) ==
    LET reached == Closure(S)
    IN IF holder \in reached /\ value # Null
       THEN Closure(reached \cup {value}) ELSE reached

\* What the mutator can still get to.
Reachable == EClosure(roots \cup locals)

TypeOK ==
    /\ fresh \subseteq Obj /\ alive \subseteq Obj /\ fresh \cap alive = {}
    /\ young \subseteq alive
    /\ edges \in [Obj -> [Fields -> Obj \cup {Null}]]
    /\ roots \subseteq Obj /\ locals \subseteq alive
    /\ phase \in {"idle", "marking", "finishing"}
    /\ marked \subseteq Obj /\ grey \subseteq marked
    /\ cursor \in [obj : Obj \cup {None}, idx : 0..NF]
    /\ buffer \subseteq Obj
    /\ holder \in Obj \cup {Null} /\ target \in Obj \cup {Null}
    /\ value \in Obj \cup {Null}
    /\ (holder = Null) = (target = Null)
    /\ holder = Null => value = Null
    /\ recorded \in BOOLEAN
    /\ phase = "idle" => marked = {} /\ buffer = {} /\ grey = {}

\* A discovered ephemeron whose key got marked (or is young): its value is
\* to be marked.
Resolvable ==
    /\ recorded /\ value # Null /\ value \notin young /\ value \notin marked
    /\ target \in marked \cup young

Done ==
    /\ phase = "finishing"
    /\ grey = {} /\ buffer = {} /\ cursor.obj = None
    /\ ~Resolvable \/ Mutation = "no_pair_fixpoint"

\* When the pause has nothing left to do, every old object still reachable,
\* through young objects too, is marked.
MarkingComplete == Done => (Reachable \ young) \subseteq marked

\* Its consequence: nothing reachable was freed.
NoDangling == Reachable \subseteq alive

\* At the end of marking, every old object with an edge to a young one (a
\* field, or the ephemeron's key or value) is marked: what the scavenges
\* read while the sweep runs alongside the mutator (the slot sets) is in no
\* object the sweep frees.
SlotsLive ==
    Done => \A o \in Old :
        (\/ \E f \in Fields : edges[o][f] \in young
         \/ o = holder /\ (target \in young \/ value \in young))
        => o \in marked

\* The ephemeron never points to a freed object: a live holder's
\* (reachable, or garbage not swept yet) is cleared first.
WeakSafe ==
    holder # Null => holder \in alive /\ target \in alive /\ value \in alive \cup {Null}

\* Violated once anything is freed: the model does collect.
NothingSwept == fresh \cup alive = Obj

Init ==
    /\ fresh = Obj
    /\ alive = {}
    /\ young = {}
    /\ edges = [o \in Obj |-> [f \in Fields |-> Null]]
    /\ roots = {}
    /\ locals = {}
    /\ phase = "idle"
    /\ marked = {}
    /\ grey = {}
    /\ cursor = NoCursor
    /\ buffer = {}
    /\ holder = Null
    /\ target = Null
    /\ value = Null
    /\ recorded = FALSE

(* The mutator, inside an operation. *)

\* In the nursery, or in the old generation (pretenured, or the nursery
\* full), black while marking runs.
Alloc ==
    /\ Mutating
    /\ \E o \in fresh, inNursery \in BOOLEAN :
        /\ fresh' = fresh \ {o}
        /\ alive' = alive \cup {o}
        /\ young' = IF inNursery THEN young \cup {o} ELSE young
        /\ locals' = locals \cup {o}
        /\ marked' = IF /\ phase = "marking" /\ ~inNursery
                        /\ Mutation # "no_black_allocation"
                     THEN marked \cup {o} ELSE marked
    /\ UNCHANGED <<edges, roots, phase, grey, cursor, buffer>>
    /\ UNCHANGED weakVars

LoadRoot ==
    /\ Mutating
    /\ \E r \in roots : locals' = locals \cup {r}
    /\ UNCHANGED <<fresh, alive, young, edges, roots, phase, marked, grey,
                   cursor, buffer>>
    /\ UNCHANGED weakVars

ReadField ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields :
        /\ edges[o][f] # Null
        /\ locals' = locals \cup {edges[o][f]}
    /\ UNCHANGED <<fresh, alive, young, edges, roots, phase, marked, grey,
                   cursor, buffer>>
    /\ UNCHANGED weakVars

\* The weak read barrier: what a weak edge or root gives the mutator
\* while marking runs is marked (through the barrier's buffer).
WeakRead(t) ==
    IF /\ phase = "marking" /\ t \notin young /\ t \notin marked
       /\ Mutation # "no_weak_read_barrier"
    THEN buffer \cup {t} ELSE buffer

\* The key, with the barrier, and the value (none for a weak edge).
ReadWeak ==
    /\ Mutating
    /\ holder \in locals
    /\ locals' = locals \cup {target} \cup ({value} \ {Null})
    /\ buffer' = WeakRead(target)
    /\ UNCHANGED <<fresh, alive, young, edges, roots, phase, marked, grey,
                   cursor, holder, target, value, recorded>>

\* The barrier records the old value overwritten, if not marked; a young
\* one, never.
Barrier(old) ==
    IF /\ phase = "marking" /\ old # Null
       /\ old \notin young /\ old \notin marked
    THEN buffer \cup {old} ELSE buffer

\* The value overwritten (or emptied) goes through the SATB barrier; the
\* key needs none (it keeps nothing alive). The heap's one ephemeron is
\* written (its holder's recorded slot stays so) or moves to another
\* holder (the old holder's emptied, the new one's not recorded).
ValueBarrier ==
    IF Mutation = "no_pair_value_barrier" THEN buffer ELSE Barrier(value)

WriteWeak ==
    /\ Mutating
    /\ \E o \in locals, k \in locals, v \in locals \cup {Null} :
        /\ holder' = o
        /\ target' = k
        /\ value' = v
        /\ recorded' = (recorded /\ o = holder)
    /\ buffer' = ValueBarrier
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase, marked,
                   grey, cursor>>

ClearWeak ==
    /\ Mutating
    /\ holder \in locals
    /\ holder' = Null /\ target' = Null /\ value' = Null /\ recorded' = FALSE
    /\ buffer' = ValueBarrier
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase, marked,
                   grey, cursor>>

Write ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields, v \in locals \cup {Null} :
        /\ edges' = [edges EXCEPT ![o][f] = v]
        /\ buffer' = IF Mutation = "no_write_barrier" THEN buffer
                     ELSE Barrier(edges[o][f])
    /\ UNCHANGED <<fresh, alive, young, roots, locals, phase, marked, grey,
                   cursor>>
    /\ UNCHANGED weakVars

AddRoot ==
    /\ Mutating
    /\ \E o \in locals : roots' = roots \cup {o}
    /\ UNCHANGED <<fresh, alive, young, edges, locals, phase, marked, grey,
                   cursor, buffer>>
    /\ UNCHANGED weakVars

DeleteRoot ==
    /\ Mutating
    /\ \E r \in roots :
        /\ roots' = roots \ {r}
        /\ buffer' = IF Mutation = "no_root_delete_barrier" THEN buffer
                     ELSE Barrier(r)
    /\ UNCHANGED <<fresh, alive, young, edges, locals, phase, marked, grey,
                   cursor>>
    /\ UNCHANGED weakVars

EndOperation ==
    /\ Mutating
    /\ locals # {}
    /\ locals' = {}
    /\ UNCHANGED <<fresh, alive, young, edges, roots, phase, marked, grey,
                   cursor, buffer>>
    /\ UNCHANGED weakVars

(* The GC, at safepoints and on the marker. *)

\* At a safepoint (the marker paused, if marking): the young objects the
\* roots and the old generation reach survive, any of them promoted
\* (black while marking runs); the others are freed.
Scavenge ==
    /\ Mutating /\ locals = {} /\ young # {}
    /\ LET survivors == young \cap SClosure(roots \cup Old) IN
       \E promoted \in SUBSET survivors :
        /\ young' = survivors \ promoted
        /\ alive' = alive \ (young \ survivors)
        /\ edges' = [o \in Obj |-> IF o \in young \ survivors
                                   THEN [f \in Fields |-> Null]
                                   ELSE edges[o]]
        /\ marked' = IF phase = "marking" /\ Mutation # "no_black_promotion"
                     THEN marked \cup promoted ELSE marked
        /\ LET freed == young \ survivors
               cleared == \/ holder \in freed
                          \/ target \in freed /\ Mutation # "no_weak_scavenge_clear"
           IN /\ holder' = IF cleared THEN Null ELSE holder
              /\ target' = IF cleared THEN Null ELSE target
              /\ value' = IF cleared THEN Null ELSE value
              /\ recorded' = (recorded /\ ~cleared)
    /\ UNCHANGED <<fresh, roots, locals, phase, grey, cursor, buffer>>

\* At a safepoint, the nursery empty (a scavenge promoting every survivor
\* just ran): every root marked.
StartMarking ==
    /\ phase = "idle" /\ locals = {}
    /\ young = {} \/ Mutation = "no_scavenge_at_start"
    /\ phase' = "marking"
    /\ marked' = roots \ young
    /\ grey' = roots \ young
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, cursor, buffer>>
    /\ UNCHANGED weakVars

TakeFromWorklist ==
    /\ Marking
    /\ cursor.obj = None
    /\ \E o \in grey :
        /\ cursor' = [obj |-> o, idx |-> 1]
        /\ LET discovered == o = holder /\ target \notin young /\ target \notin marked
                \* The value marked through: the key is marked, or young.
                through == /\ o = holder /\ ~discovered /\ value # Null
                           /\ value \notin young /\ value \notin marked
           IN /\ recorded' = (recorded \/ discovered)
              /\ marked' = IF through THEN marked \cup {value} ELSE marked
              /\ grey' = (grey \ {o}) \cup (IF through THEN {value} ELSE {})
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase,
                   buffer, holder, target, value>>

\* A discovered ephemeron whose key got marked: its value marked.
ResolvePair ==
    /\ Marking
    /\ Resolvable
    /\ Mutation # "no_pair_fixpoint"
    /\ marked' = marked \cup {value}
    /\ grey' = grey \cup {value}
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase, cursor,
                   buffer, holder, target, value, recorded>>

\* One field of the object being scanned: an old target not marked yet is
\* marked; a young one is skipped.
ScanField ==
    /\ Marking
    /\ cursor.obj # None
    /\ LET t == edges[cursor.obj][cursor.idx] IN
        /\ IF t # Null /\ t \notin young /\ t \notin marked
           THEN /\ marked' = marked \cup {t}
                /\ grey' = grey \cup {t}
           ELSE UNCHANGED <<marked, grey>>
        /\ cursor' = IF cursor.idx = NF THEN NoCursor
                     ELSE [cursor EXCEPT !.idx = @ + 1]
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase, buffer>>
    /\ UNCHANGED weakVars

DrainBuffer ==
    /\ Marking
    /\ buffer # {}
    /\ marked' = marked \cup buffer
    /\ grey' = grey \cup (buffer \ marked)
    /\ buffer' = {}
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase, cursor>>
    /\ UNCHANGED weakVars

BeginFinish ==
    /\ phase = "marking" /\ locals = {}
    /\ phase' = "finishing"
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, marked, grey,
                   cursor, buffer>>
    /\ UNCHANGED weakVars

\* The recorded objects' weak edges to old objects not marked are cleared,
\* and the weak roots to them; the old generation's unmarked are freed;
\* the nursery stays.
Dead(t) == t # Null /\ t \notin young /\ t \notin marked

Sweep ==
    /\ Done
    /\ LET kept == young \cup (Old \cap marked) IN
        /\ alive' = kept
        /\ edges' = [o \in Obj |-> IF o \in kept
                                   THEN edges[o] ELSE [f \in Fields |-> Null]]
        /\ LET cleared == \/ holder # Null /\ holder \notin kept
                          \/ /\ recorded /\ Dead(target)
                             /\ Mutation # "no_weak_clear"
           IN /\ holder' = IF cleared THEN Null ELSE holder
              /\ target' = IF cleared THEN Null ELSE target
              /\ value' = IF cleared THEN Null ELSE value
    /\ marked' = {}
    /\ recorded' = FALSE
    /\ phase' = "idle"
    /\ UNCHANGED <<fresh, young, roots, locals, grey, cursor, buffer>>

Next ==
    \/ Alloc \/ LoadRoot \/ ReadField \/ Write \/ AddRoot \/ DeleteRoot
    \/ EndOperation
    \/ ReadWeak \/ WriteWeak \/ ClearWeak
    \/ Scavenge \/ StartMarking \/ TakeFromWorklist \/ ScanField \/ ResolvePair
    \/ DrainBuffer \/ BeginFinish \/ Sweep

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Obj)
=============================================================================
