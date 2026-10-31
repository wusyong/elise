------------------------------ MODULE Unified ------------------------------
(***************************************************************************)
(* elise's heap and V8's marking together in one major GC, as cppgc and    *)
(* V8 do in Chrome's unified heap (cppgc's model). elise's side is         *)
(* ../marking/SatbYoung.tla without the ephemeron (checked there):         *)
(* snapshot at the beginning, black allocation and promotion, a nursery    *)
(* that scavenges, a marking that starts with the nursery empty.           *)
(*                                                                         *)
(* V8's side is a heap of JS objects, one field each, marked with a       *)
(* Dijkstra barrier, black allocation and a rescan of its roots in the    *)
(* pause; and its scavenge, which drops the wrappers nothing but their    *)
(* node holds that were never modified (V8's reclaim_unmodified_wrappers, *)
(* their TracedReferences droppable).                                      *)
(*                                                                         *)
(* Between them, as cppgc's objects and their wrappers:                    *)
(* - A node's JsEdge is its wrapper (a TracedReference in the node). The  *)
(*   new heap's marker, scanning the node, hands it to V8. When V8's      *)
(*   scavenge drops the wrapper, it resets the node's JsEdge (ResetRoot). *)
(* - A wrapper holds its node's address: only old objects are wrapped    *)
(*   (the types with a JsEdge are allocated old), which never move.       *)
(*   V8's marker, scanning the wrapper, hands the node to the new heap's  *)
(*   marker. A scavenge of the new heap takes nothing from wrappers.      *)
(* - Unwrapping reads the node with a barrier: the new heap's snapshot    *)
(*   doesn't hold what only a wrapper held, and V8 sees no barrier when   *)
(*   script drops the wrapper.                                             *)
(* - Wrappers are made outside the new heap's operations (V8 may advance  *)
(*   marking or scavenge as it allocates), of a rooted node; the          *)
(*   operation that unwraps or reads a node's wrapper is one, so V8's     *)
(*   pause and scavenge, entered only between operations, see no raw      *)
(*   pointers.                                                             *)
(* - The pause ends when both sides are done.                              *)
(*                                                                         *)
(* `Mutation` takes one part out; check.py lists the counterexample each  *)
(* gives.                                                                  *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Obj,        \* the new heap's objects (model values)
    Js,         \* V8's objects
    NF,         \* fields per object of the new heap
    Null,       \* an empty field
    None,       \* the marker scanning no object
    Mutation    \* "none", or the part of the protocol taken out

Mutations == {"none", "no_write_barrier", "no_black_allocation",
              "no_black_promotion", "young_wrapped_nodes",
              "no_wrapper_handoff", "no_js_edge_handoff", "no_unwrap_barrier",
              "no_js_write_barrier", "no_js_root_rescan",
              "finish_on_v8_alone", "finish_on_new_heap_alone",
              "no_reset_root", "drop_modified", "drop_reachable"}

ASSUME Mutation \in Mutations
ASSUME NF \in Nat \ {0}
ASSUME Null \notin Obj \cup Js /\ None \notin Obj /\ Obj \cap Js = {}

Fields == 1..NF
\* The fields the marker scans: the node's fields, then its JsEdge.
JsEdgeIdx == NF + 1
NoCursor == [obj |-> None, idx |-> 0]

VARIABLES
    fresh,      \* never allocated
    alive,      \* allocated and not freed
    young,      \* the nursery's (alive)
    edges,      \* each object's fields
    jsref,      \* each object's JsEdge: its wrapper, or Null
    roots,      \* the root table (Persistent)
    locals,     \* the current operation's raw pointers
    phase,      \* "idle", "marking" or "finishing" (the pause)
    marked,     \* mark bits (old objects')
    grey,       \* the marker's worklist
    cursor,     \* the object the marker is scanning and its next field
    buffer,     \* the barrier's buffer
    jsFresh,    \* V8's: never allocated
    jsAlive,    \* allocated and not freed
    jsEdges,    \* each JS object's one field (a wrapper's: an expando)
    jsRoots,    \* what script holds (its stack and globals)
    jsMarked,   \* V8's mark bits
    jsGrey,     \* V8's worklist
    modified,   \* the JS objects script wrote a field of (V8: not unmodified)
    wraps,      \* each wrapper's node, or Null
    lost,       \* the nodes whose modified wrapper was dropped (history)
    wrapped     \* every object that ever had a wrapper (history)

vars == <<fresh, alive, young, edges, jsref, roots, locals, phase, marked,
          grey, cursor, buffer, jsFresh, jsAlive, jsEdges, jsRoots, jsMarked,
          jsGrey, modified, wraps, lost, wrapped>>
heapVars == <<fresh, alive, young, edges, jsref, roots, locals, phase,
              marked, grey, cursor, buffer>>
jsVars == <<jsFresh, jsAlive, jsEdges, jsRoots, jsMarked, jsGrey, modified>>
bridgeVars == <<wraps, lost, wrapped>>

Mutating == phase \in {"idle", "marking"}
Marking == phase \in {"marking", "finishing"}
Old == alive \ young

\* What the mutator can get to from `s` (the new heap's) and `j` (V8's),
\* across both: fields, JsEdges, JS fields and wrappers' nodes.
RECURSIVE Reach(_, _)
Reach(s, j) ==
    LET s2 == s \cup (({edges[o][f] : o \in s, f \in Fields}
                       \cup {wraps[w] : w \in j}) \ {Null})
        j2 == j \cup (({jsref[o] : o \in s} \cup {jsEdges[w] : w \in j}) \ {Null})
    IN IF s2 = s /\ j2 = j THEN <<s, j>> ELSE Reach(s2, j2)

Reachable == Reach(roots \cup locals, jsRoots)
ReachableObj == Reachable[1]
ReachableJs == Reachable[2]

\* Through the new heap's fields only: what a scavenge keeps from `s`.
RECURSIVE Closure(_)
Closure(s) ==
    LET next == s \cup ({edges[o][f] : o \in s, f \in Fields} \ {Null})
    IN IF next = s THEN s ELSE Closure(next)

\* Through JS fields only: what V8's scavenge keeps from `j`.
RECURSIVE JsClosure(_)
JsClosure(j) ==
    LET next == j \cup ({jsEdges[w] : w \in j} \ {Null})
    IN IF next = j THEN j ELSE JsClosure(next)

TypeOK ==
    /\ fresh \subseteq Obj /\ alive \subseteq Obj /\ fresh \cap alive = {}
    /\ young \subseteq alive
    /\ edges \in [Obj -> [Fields -> Obj \cup {Null}]]
    /\ jsref \in [Obj -> Js \cup {Null}]
    /\ roots \subseteq Obj /\ locals \subseteq Obj
    /\ phase \in {"idle", "marking", "finishing"}
    /\ marked \subseteq Obj /\ grey \subseteq marked
    /\ cursor \in [obj : Obj \cup {None}, idx : 0..JsEdgeIdx]
    /\ buffer \subseteq Obj
    /\ jsFresh \subseteq Js /\ jsAlive \subseteq Js /\ jsFresh \cap jsAlive = {}
    /\ jsEdges \in [Js -> Js \cup {Null}]
    /\ jsRoots \subseteq Js
    /\ jsMarked \subseteq Js /\ jsGrey \subseteq jsMarked
    /\ modified \subseteq Js
    /\ wraps \in [Js -> Obj \cup {Null}]
    /\ lost \subseteq Obj /\ wrapped \subseteq Obj
    /\ phase = "idle" => marked = {} /\ buffer = {} /\ grey = {}
                         /\ jsMarked = {} /\ jsGrey = {}

NewHeapDone == grey = {} /\ buffer = {} /\ cursor.obj = None
V8Done == jsGrey = {} /\ (jsRoots \subseteq jsMarked \/ Mutation = "no_js_root_rescan")

Done ==
    /\ phase = "finishing"
    /\ \/ NewHeapDone /\ V8Done
       \/ Mutation = "finish_on_v8_alone" /\ V8Done
       \/ Mutation = "finish_on_new_heap_alone" /\ NewHeapDone

\* When the pause has nothing left to do, every old object and every JS
\* object still reachable is marked.
MarkingComplete ==
    Done => /\ (ReachableObj \ young) \subseteq marked
            /\ ReachableJs \subseteq jsMarked

\* Nothing reachable was freed, on either side.
NoDangling == ReachableObj \subseteq alive /\ ReachableJs \subseteq jsAlive

\* A wrapper's node is old: its address, which the wrapper holds, never
\* changes (the nursery's objects move as they survive).
WrappedNotYoung == \A j \in Js : wraps[j] # Null => wraps[j] \notin young

\* No wrapper script modified (its expandos) was ever dropped.
ExpandosKept == lost = {}

\* Violated once a node that had a wrapper is freed: the cycle of a node
\* and its wrapper is collected.
WrappedNeverFreed == wrapped \subseteq alive

Init ==
    /\ fresh = Obj
    /\ alive = {}
    /\ young = {}
    /\ edges = [o \in Obj |-> [f \in Fields |-> Null]]
    /\ jsref = [o \in Obj |-> Null]
    /\ roots = {}
    /\ locals = {}
    /\ phase = "idle"
    /\ marked = {}
    /\ grey = {}
    /\ cursor = NoCursor
    /\ buffer = {}
    /\ jsFresh = Js
    /\ jsAlive = {}
    /\ jsEdges = [j \in Js |-> Null]
    /\ jsRoots = {}
    /\ jsMarked = {}
    /\ jsGrey = {}
    /\ modified = {}
    /\ wraps = [j \in Js |-> Null]
    /\ lost = {}
    /\ wrapped = {}

(* The new heap's mutator, inside an operation (SatbYoung's). *)

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
    /\ UNCHANGED <<edges, jsref, roots, phase, grey, cursor, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

LoadRoot ==
    /\ Mutating
    /\ \E r \in roots : locals' = locals \cup {r}
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, phase, marked,
                   grey, cursor, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

ReadField ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields :
        /\ edges[o][f] # Null
        /\ locals' = locals \cup {edges[o][f]}
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, phase, marked,
                   grey, cursor, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

\* The SATB barrier: the old value overwritten, if old and not marked.
Barrier(old) ==
    IF /\ phase = "marking" /\ old # Null
       /\ old \notin young /\ old \notin marked
    THEN buffer \cup {old} ELSE buffer

Write ==
    /\ Mutating
    /\ \E o \in locals, f \in Fields, v \in locals \cup {Null} :
        /\ edges' = [edges EXCEPT ![o][f] = v]
        /\ buffer' = IF Mutation = "no_write_barrier" THEN buffer
                     ELSE Barrier(edges[o][f])
    /\ UNCHANGED <<fresh, alive, young, jsref, roots, locals, phase, marked,
                   grey, cursor>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

AddRoot ==
    /\ Mutating
    /\ \E o \in locals : roots' = roots \cup {o}
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, locals, phase, marked,
                   grey, cursor, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

DeleteRoot ==
    /\ Mutating
    /\ \E r \in roots :
        /\ roots' = roots \ {r}
        /\ buffer' = Barrier(r)
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, locals, phase, marked,
                   grey, cursor>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

EndOperation ==
    /\ Mutating
    /\ locals # {}
    /\ locals' = {}
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, phase, marked,
                   grey, cursor, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

(* Script: V8's mutator, any time but in the pause. *)

\* A plain JS object, black while marking runs.
JsAlloc ==
    /\ Mutating
    /\ \E j \in jsFresh :
        /\ jsFresh' = jsFresh \ {j}
        /\ jsAlive' = jsAlive \cup {j}
        /\ jsRoots' = jsRoots \cup {j}
        /\ jsMarked' = IF phase = "marking" THEN jsMarked \cup {j} ELSE jsMarked
    /\ UNCHANGED <<jsEdges, jsGrey, modified>>
    /\ UNCHANGED heapVars /\ UNCHANGED bridgeVars

JsLoad ==
    /\ Mutating
    /\ \E j \in jsRoots :
        /\ jsEdges[j] # Null
        /\ jsRoots' = jsRoots \cup {jsEdges[j]}
    /\ UNCHANGED <<jsFresh, jsAlive, jsEdges, jsMarked, jsGrey, modified>>
    /\ UNCHANGED heapVars /\ UNCHANGED bridgeVars

\* V8's Dijkstra barrier: the new value, if not marked. Writing a field
\* modifies the object (on a wrapper: an expando).
JsWrite ==
    /\ Mutating
    /\ \E j \in jsRoots, v \in jsRoots \cup {Null} :
        /\ jsEdges' = [jsEdges EXCEPT ![j] = v]
        /\ modified' = IF v # Null THEN modified \cup {j} ELSE modified
        /\ IF /\ phase = "marking" /\ v # Null /\ v \notin jsMarked
              /\ Mutation # "no_js_write_barrier"
           THEN /\ jsMarked' = jsMarked \cup {v}
                /\ jsGrey' = jsGrey \cup {v}
           ELSE UNCHANGED <<jsMarked, jsGrey>>
    /\ UNCHANGED <<jsFresh, jsAlive, jsRoots>>
    /\ UNCHANGED heapVars /\ UNCHANGED bridgeVars

\* No barrier: V8 rescans its roots in the pause.
JsDrop ==
    /\ Mutating
    /\ \E j \in jsRoots : jsRoots' = jsRoots \ {j}
    /\ UNCHANGED <<jsFresh, jsAlive, jsEdges, jsMarked, jsGrey, modified>>
    /\ UNCHANGED heapVars /\ UNCHANGED bridgeVars

(* The bindings. *)

\* A node's first wrapper, made outside an operation of a rooted, old node
\* (its type allocated old): black (V8's allocation), given to script. The
\* JsEdge written is empty before.
Wrap ==
    /\ Mutating /\ locals = {}
    /\ \E n \in roots, j \in jsFresh :
        /\ jsref[n] = Null
        /\ n \notin young \/ Mutation = "young_wrapped_nodes"
        /\ jsFresh' = jsFresh \ {j}
        /\ jsAlive' = jsAlive \cup {j}
        /\ jsRoots' = jsRoots \cup {j}
        /\ jsMarked' = IF phase = "marking" THEN jsMarked \cup {j} ELSE jsMarked
        /\ jsref' = [jsref EXCEPT ![n] = j]
        /\ wraps' = [wraps EXCEPT ![j] = n]
        /\ wrapped' = wrapped \cup {n}
    /\ UNCHANGED <<jsEdges, jsGrey, modified, lost>>
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase, marked,
                   grey, cursor, buffer>>

\* In an operation: the wrapper's node, read with the barrier.
Unwrap ==
    /\ Mutating
    /\ \E j \in jsRoots :
        /\ wraps[j] # Null
        /\ locals' = locals \cup {wraps[j]}
        /\ buffer' = IF Mutation = "no_unwrap_barrier" THEN buffer
                     ELSE Barrier(wraps[j])
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, phase, marked,
                   grey, cursor>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

\* In an operation: the node's wrapper, given to script (a new root of
\* V8's, which it rescans in the pause).
GetWrapper ==
    /\ Mutating
    /\ \E n \in locals :
        /\ jsref[n] # Null
        /\ jsRoots' = jsRoots \cup {jsref[n]}
    /\ UNCHANGED <<jsFresh, jsAlive, jsEdges, jsMarked, jsGrey, modified>>
    /\ UNCHANGED heapVars /\ UNCHANGED bridgeVars

(* The GCs. *)

\* The new heap's, at a safepoint: the young objects the roots and the old
\* generation reach survive, any of them promoted (black while marking
\* runs); the others are freed. Wrappers hold only old nodes.
Scavenge ==
    /\ Mutating /\ locals = {} /\ young # {}
    /\ LET survivors == young \cap Closure(roots \cup Old)
           freed == young \ survivors
       IN \E promoted \in SUBSET survivors :
        /\ young' = survivors \ promoted
        /\ alive' = alive \ freed
        /\ edges' = [o \in Obj |-> IF o \in freed
                                   THEN [f \in Fields |-> Null] ELSE edges[o]]
        /\ jsref' = [o \in Obj |-> IF o \in freed THEN Null ELSE jsref[o]]
        /\ marked' = IF phase = "marking" /\ Mutation # "no_black_promotion"
                     THEN marked \cup promoted ELSE marked
    /\ UNCHANGED <<fresh, roots, locals, phase, grey, cursor, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

\* V8's, between operations (while marking runs too): its roots are what
\* script holds and the JsEdges (TracedReferences) of the live objects,
\* but those to wrappers never modified (droppable, unmodified). What
\* they don't reach is freed; a dropped wrapper's node gets its JsEdge
\* reset (EmbedderRootsHandler::ResetRoot).
JsScavenge ==
    /\ Mutating /\ locals = {}
    /\ LET traced == {jsref[o] : o \in alive} \ {Null}
           strong == IF Mutation = "drop_modified" THEN {}
                     ELSE traced \cap modified
           kept == IF Mutation = "drop_reachable"
                   THEN JsClosure(jsRoots \cup strong) \ (traced \ modified)
                   ELSE JsClosure(jsRoots \cup strong)
           freed == jsAlive \ kept
       IN /\ freed # {}
          /\ jsAlive' = jsAlive \ freed
          /\ jsEdges' = [j \in Js |-> IF j \in freed THEN Null ELSE jsEdges[j]]
          /\ jsMarked' = jsMarked \ freed
          /\ jsGrey' = jsGrey \ freed
          /\ modified' = modified \ freed
          /\ wraps' = [j \in Js |-> IF j \in freed THEN Null ELSE wraps[j]]
          /\ jsref' = IF Mutation = "no_reset_root" THEN jsref
                      ELSE [o \in Obj |-> IF jsref[o] \in freed THEN Null
                                          ELSE jsref[o]]
          /\ lost' = lost \cup ({wraps[j] : j \in freed \cap modified} \ {Null})
    /\ UNCHANGED <<jsFresh, jsRoots, wrapped>>
    /\ UNCHANGED <<fresh, alive, young, edges, roots, locals, phase, marked,
                   grey, cursor, buffer>>

\* V8 starts a major GC between operations; the new heap's nursery is
\* empty (a scavenge promoting every survivor just ran). Each side marks
\* its roots.
StartMarking ==
    /\ phase = "idle" /\ locals = {}
    /\ young = {}
    /\ phase' = "marking"
    /\ marked' = roots
    /\ grey' = roots
    /\ jsMarked' = jsRoots
    /\ jsGrey' = jsRoots
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, locals, cursor,
                   buffer>>
    /\ UNCHANGED <<jsFresh, jsAlive, jsEdges, jsRoots, modified>>
    /\ UNCHANGED bridgeVars

TakeFromWorklist ==
    /\ Marking
    /\ cursor.obj = None
    /\ \E o \in grey :
        /\ cursor' = [obj |-> o, idx |-> 1]
        /\ grey' = grey \ {o}
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, locals, phase,
                   marked, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

\* One field of the object being scanned: an old target not marked yet is
\* marked, a young one skipped; its JsEdge last, handed to V8.
ScanField ==
    /\ Marking
    /\ cursor.obj # None
    /\ IF cursor.idx = JsEdgeIdx
       THEN LET j == jsref[cursor.obj] IN
            /\ IF /\ j # Null /\ j \notin jsMarked
                  /\ Mutation # "no_js_edge_handoff"
               THEN /\ jsMarked' = jsMarked \cup {j}
                    /\ jsGrey' = jsGrey \cup {j}
               ELSE UNCHANGED <<jsMarked, jsGrey>>
            /\ UNCHANGED <<marked, grey>>
       ELSE LET t == edges[cursor.obj][cursor.idx] IN
            /\ IF t # Null /\ t \notin young /\ t \notin marked
               THEN /\ marked' = marked \cup {t}
                    /\ grey' = grey \cup {t}
               ELSE UNCHANGED <<marked, grey>>
            /\ UNCHANGED <<jsMarked, jsGrey>>
    /\ cursor' = IF cursor.idx = JsEdgeIdx THEN NoCursor
                 ELSE [cursor EXCEPT !.idx = @ + 1]
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, locals, phase,
                   buffer>>
    /\ UNCHANGED <<jsFresh, jsAlive, jsEdges, jsRoots, modified>>
    /\ UNCHANGED bridgeVars

DrainBuffer ==
    /\ Marking
    /\ buffer # {}
    /\ marked' = marked \cup buffer
    /\ grey' = grey \cup (buffer \ marked)
    /\ buffer' = {}
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, locals, phase,
                   cursor>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

\* V8's marker scans a JS object: its field, and a wrapper's node, handed
\* to the new heap's worklist (marked, if old and not yet).
V8Scan ==
    /\ Marking
    /\ \E j \in jsGrey :
        /\ jsGrey' = (jsGrey \ {j}) \cup
                     (IF jsEdges[j] # Null /\ jsEdges[j] \notin jsMarked
                      THEN {jsEdges[j]} ELSE {})
        /\ jsMarked' = jsMarked \cup ({jsEdges[j]} \ {Null})
        /\ LET n == wraps[j] IN
           IF /\ n # Null /\ n \notin young /\ n \notin marked
              /\ Mutation # "no_wrapper_handoff"
           THEN /\ marked' = marked \cup {n}
                /\ grey' = grey \cup {n}
           ELSE UNCHANGED <<marked, grey>>
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, locals, phase,
                   cursor, buffer>>
    /\ UNCHANGED <<jsFresh, jsAlive, jsEdges, jsRoots, modified>>
    /\ UNCHANGED bridgeVars

\* V8 enters the pause between operations.
BeginFinish ==
    /\ phase = "marking" /\ locals = {}
    /\ phase' = "finishing"
    /\ UNCHANGED <<fresh, alive, young, edges, jsref, roots, locals, marked,
                   grey, cursor, buffer>>
    /\ UNCHANGED jsVars /\ UNCHANGED bridgeVars

\* In the pause, V8 rescans its roots (its barrier doesn't cover them).
RescanJsRoots ==
    /\ phase = "finishing"
    /\ Mutation # "no_js_root_rescan"
    /\ ~(jsRoots \subseteq jsMarked)
    /\ jsMarked' = jsMarked \cup jsRoots
    /\ jsGrey' = jsGrey \cup (jsRoots \ jsMarked)
    /\ UNCHANGED <<jsFresh, jsAlive, jsEdges, jsRoots, modified>>
    /\ UNCHANGED heapVars /\ UNCHANGED bridgeVars

\* Both sides sweep: the new heap's old unmarked objects and V8's unmarked
\* ones are freed, a freed node's JsEdge with it (V8 already let its
\* TracedReference go), a freed wrapper's node pointer with it.
Sweep ==
    /\ Done
    /\ LET kept == young \cup (Old \cap marked)
           jsKept == jsAlive \cap jsMarked
       IN /\ alive' = kept
          /\ edges' = [o \in Obj |-> IF o \in kept
                                     THEN edges[o] ELSE [f \in Fields |-> Null]]
          /\ jsref' = [o \in Obj |-> IF o \in kept THEN jsref[o] ELSE Null]
          /\ jsAlive' = jsKept
          /\ jsEdges' = [j \in Js |-> IF j \in jsKept THEN jsEdges[j] ELSE Null]
          /\ modified' = modified \cap jsKept
          /\ wraps' = [j \in Js |-> IF j \in jsKept THEN wraps[j] ELSE Null]
    /\ phase' = "idle"
    /\ marked' = {} /\ grey' = {} /\ cursor' = NoCursor /\ buffer' = {}
    /\ jsMarked' = {} /\ jsGrey' = {}
    /\ UNCHANGED <<fresh, young, roots, locals>>
    /\ UNCHANGED <<jsFresh, jsRoots>>
    /\ UNCHANGED <<lost, wrapped>>

Next ==
    \/ Alloc \/ LoadRoot \/ ReadField \/ Write \/ AddRoot \/ DeleteRoot
    \/ EndOperation
    \/ JsAlloc \/ JsLoad \/ JsWrite \/ JsDrop
    \/ Wrap \/ Unwrap \/ GetWrapper
    \/ Scavenge \/ JsScavenge \/ StartMarking \/ TakeFromWorklist \/ ScanField
    \/ DrainBuffer \/ V8Scan \/ BeginFinish \/ RescanJsRoots \/ Sweep

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Obj) \cup Permutations(Js)
=============================================================================
