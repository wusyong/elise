# 測試

## 指令

Toolchain 釘在 Rust 1.99.0（`rust-toolchain.toml`，和 workspace 的 `rust-version` 相同），rustup 會自動安裝。

```sh
cargo test --workspace                # 單元測試、整合測試（tests/it/）、compile-fail（tests/ui/）
python3 scripts/sanity.py tests       # 每個 crate 的整合測試是一個 binary（tests/it/）
python3 scripts/sanity.py numerics    # 整數運算寫明溢位時怎麼辦（contributing.md）
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test` 以外的，各自要另外裝工具（`test.py <指令> -h` 寫了要裝什麼）：

```sh
python3 scripts/test.py models                           # 平行的程式：loom、shuttle
python3 scripts/test.py fuzz [--minutes N] [--seeds DIR] # fuzz（cargo-fuzz，預設 10 分鐘）
python3 scripts/test.py verus                            # Verus 的證明
python3 scripts/test.py miri [NEXTEST_ARG ...]           # Miri（cargo-nextest）
python3 tla/marking/check.py                             # TLA+ 的模型（下面的「TLA+ 的模型」）
python3 tla/unified/check.py
```

fuzz 和 Miri 用 `crates/elise/fuzz/rust-toolchain.toml` 釘的 nightly，兩個一起升級；Verus 的版本在 `test.py` 的 `VERUS_VERSION`，和 `Cargo.toml` 釘的 `vstd` 一起升級。升級 toolchain 之後用 `TRYBUILD=overwrite cargo test -p elise --test compile_fail` 重新產生 `crates/elise/tests/ui/` 的 `.stderr`，確認錯誤的種類和位置沒變。

## CI

GitHub Actions，兩個 workflow：

| workflow | 什麼時候 | 跑什麼 |
| --- | --- | --- |
| `.github/workflows/ci.yml` | 每次 push 和 pull request，加上每天 02:00 UTC | `cargo test --workspace`（Linux、macOS、Windows）；fmt、clippy、`sanity.py`；`test.py models` |
| `.github/workflows/scheduled.yml` | 每天 03:00 UTC | `test.py fuzz --minutes 30`（corpus 用 Actions 的 cache 一次接一次累積，失敗時輸入存成 artifact）、`test.py verus`（Verus 的 release 下載一次、cache 起來）、兩個 TLA+ 的 `check.py` |
| | 每週日 04:00 UTC | `test.py miri`，用 `--partition` 分給 6 台機器（每個 job 最多 6 小時） |

`scheduled.yml` 也可以手動跑（Actions → Scheduled → Run workflow），選全部或其中一項。排程只在預設的 branch 上跑；公開的 repo 連續 60 天沒有活動時，GitHub 會停掉排程，要到 Actions 頁面重新打開。同一個 branch 的新 push 會取消還在跑的舊的那次（排程的不取消）。

## Miri

`test.py miri` 跑的是

```sh
MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-disable-isolation" cargo +nightly-2026-10-01 miri nextest run -p elise --lib --test it
```

cargo-nextest 平行跑，每個測試一個 process（要先 `cargo install cargo-nextest`），之後再單獨跑刻意 leak 的那一個（下面）。給了 nextest 的參數（例如篩選）時只跑第一段。`--partition I/N` 只跑 N 份裡的第 I 份（nextest 的 `count:I/N`），讓幾台機器分著跑（CI 分成 6 份）；刻意 leak 的那一個跟著第 1 份跑。

Miri 用預設的 Stacked Borrows，定期跑一次全部（不在每次改動時跑）。Miri 一個物件一個物件地解譯，配置、copy 和 finalize 一個物件要好幾毫秒，所以測試在 Miri 下縮小重複的部分，但留下走過的路徑和規模的門檻（nursery 一頁一頁長大、large object、頁的重用）：

- 用 bytes 判斷的測試用大的物件湊出一樣的 bytes：nursery 長大和縮小的兩個測試（`young.rs`）用 480 bytes 的 node，原生也一樣。
- 只要 nursery 滿了、做了 scavenge 的測試（`js.rs` 的兩個），在 Miri 下用一頁的 nursery、分成小的操作，做到要的 scavenge 就停。
- 寫好的操作（`fuzz.rs`）在 Miri 下只跑選項的 6 組組合，每兩個選項的四種組合都有（`PAIRWISE`，有測試檢查）；三個以上的選項一起的效果由原生的全部 64 組守著。
- 依賴真實時間的檢查在 Miri 下放寬：schedule 用的 500 ms（step 的次數）、等 worker 做完的上限。

在 Miri 下縮小或放寬的地方都用 `cfg!(miri)` 標出來、寫明理由。縮小之後最慢的一個（`fuzz::a_written_run_with_sites_agrees_with_the_model`）要 40 分鐘左右，縮小以前好幾個要 1 到 3 小時以上。`-Zmiri-disable-isolation` 讓它讀得到 `crates/elise/fuzz/regressions/` 的檔案，也讓它看到真的時間（上面那些檢查用到；測試不依賴亂數）。Windows 上的 Miri 不支援列出目錄，`fuzz::the_fuzzers_regressions_pass` 在那裡失敗；CI 在 Linux 上跑。它報了像是誤報的錯時，才在 `MIRIFLAGS` 加上 `-Zmiri-tree-borrows` 用 Tree Borrows 確認。`heap::a_root_outliving_its_heap_is_caught` 刻意留下 heap 的 root 表（root 活得比 heap 久時的做法），Miri 的 leak 檢查會報它，所以在 Miri 下 ignore；要看它沒有 UB，單獨用 `-Zmiri-ignore-leaks` 跑（`--run-ignored all -E 'test(=heap::a_root_outliving_its_heap_is_caught)'`）。

## loom、shuttle

`python3 scripts/test.py models`：`--cfg loom`、`--cfg shuttle` 各一次，在 `target/loom`、`target/shuttle`（Apple M4 上約 10 秒；Windows 11 上 shuttle 約 1 分鐘）。`elise::sync` 照 cfg 把 atomic、mutex、condvar 和 thread 換成它們的。改到 marking、worklist、platform 時跑它。

## 測試的清單

- `crates/elise/tests/it/model.rs`：一個小的 DOM（parent、first child、next sibling、`HeapVector` 的 attributes、`LocalExternal` 的 style 和記下 id 的 finalizer）：每個欄位寫進去、樹在 full GC 時升到 old、之後原地不動、拆下來的子樹 finalize 一次、`External` 和物件一起放掉、`HeapVector` 長大和移除、`Persistent` 的格子、另一個 heap 的 `Member` 和 `Persistent` panic、panic 的操作之後 heap 還是好的、隨機改一棵樹（每個安全點輪流 scavenge 和 full GC）並和模型比對。
- `crates/elise/tests/it/heap.rs`（old 的頁的測試用沒有 nursery 的 heap）：空格子重用、活的物件之間的空區段從低的位址拿、先用空格子最少的頁、`stats` 的格子和頁、大的物件和 large object 活過 GC（large object 的 finalizer 一次、只 commit 用到的）、`HeapVector` 長成 large object、清空的 `HeapVector` 留著容量（重填不配置）、不留住清掉的物件、空頁進 pool、被別的級拿走、過一輪 GC 才 discard、discard 過的頁拿回來時重新 commit（沒有 worker 的 platform）、platform 有 worker 時交給 worker（還完之前 heap 不拿它、committed 不減，還完之後的安全點才拿回來，先於新的頁）、死掉的 large object 的頁給小物件用、安全點的 GC 門檻；一次 GC 的紀錄（階段照順序交給 platform 的 trace、標到的物件和 bytes 和留下的一樣）；`[Member<T>; N]` 和 `Member<Erased>` 的邊留住它們指向的物件，清掉之後放掉；`Member<Erased>` 讀回的物件只轉得回它自己的型別（GC 前後都是）；欄位叫 derive 自己的參數名稱（`init`、`slot`）時照樣寫在它的位置上。
- `crates/elise/tests/it/young.rs`（另外：nursery 剛好滿到 vector 本身配置在 old 時，它指向 young 的 store 也記下了；nursery 隨活下來的長大、不超過上限，垃圾之後 full GC 減半、不小於一開始，full GC 自己的 scavenge 不算，縮小之後累積重新算起；`pretenured` 的物件一開始就是 old、它指向 young 的邊有記下，closure panic 之後回到 nursery）：scavenge 只 copy 活的（物件搬了、值不變、一樣大）、死的 finalizer 跑一次；第二次活下來才升到 old、之後不再搬；old 物件經過 `Member::set`、`HeapVector` 的 push、set、remove、配置時的 init（large object、nursery 滿了之後配置在 old 的）指向 young 的邊留住它，清掉之後放掉；nursery 滿了之後下一個安全點做 scavenge；full GC 清空 nursery；heap drop 時 nursery 的 finalizer 跑一次。
- `crates/elise/tests/it/sites.rs`：活下來的 site 在 nursery 夠大時 tenure、死的不 tenure，兩個 site 分開算；小的 nursery 先記成「可能」，長大之後才 tenure；只有第一次活下來的算（老的升上去不算）；full GC 撤回 old 的物件死光的 site、留下活著的；`pretenured` 比 site 優先；物件的 site 經過 copy 和升級還在（標記時各算各的）。heap 在 process 共用的 pool 上 scavenge 和標記，thread 的計數合起來要對；每個 heap 開著 verifier（它檢查每個物件的 flags 和 site）。`crates/elise/src/site.rs` 的單元測試：決定的門檻、100 個以下不判斷、「可能」的轉換、撤回的計算、編號用完之後是 0。
- `crates/elise/tests/it/fragmentation.rs`（沒有 nursery 的 heap）：頁上一成的格子活著時照 V8 的挑法全部搬空、擠到一頁、放掉三頁，一半或全部活著時不搬；頁的格子、空的和 `used_bytes` 的帳對得上；每次 sweep 做完和每次 marking 開始各量一次、沒打開時不量；marking 開始時量到 sweep 之後配置填掉的空格子（垃圾填滿的頁那時不搬，sweep 之後又一樣空）；配置在隔一格一個活物件的頁上一次拿一格的區段，新的頁一次拿整頁。
- `crates/elise/tests/it/sweep.rs`（測試自己的 platform：sweep 的 job 只在測試叫它時跑）：marking 收尾之後頁等著 sweep，`used` 在 sweep 做完之前含還沒掃的垃圾、做完是 sweep 找到的活的量（也是 GC 的 marked bytes），配置時那一級沒有掃好的頁就自己掃一頁（只掃那一級的，目前的頁的空區段用完才掃）；job 掃完之後下一個安全點（`scavenge` 的也是）套用、sweep 結束（碎片在這時才量，concurrent marking 開始時也量，heap 的 thread 正在 bump 的那段算空的，nursery 的頁不算 old 的 committed），之後配置不再自己掃，先拿 sweep 找到空格子的頁；下一輪 marking 開始和 `collect` 在停頓裡掃完剩下的；sweep 期間的 scavenge 不升到還沒掃的頁上；有 `LocalExternal` 的物件（和 large object）不在收尾的停頓裡 finalize，下一個安全點才 finalize，同一頁上 `Send` 的也一起；`External` 的物件（和 large object）由 job finalize，配置同一級時不拿那一頁，large object 的頁之後被重用；job 上的 finalizer panic 時往上拋、那一頁之後重掃、不 finalize 兩次；真的 pool 上 `External` 的 `Drop` 在 worker 上跑；`collect` 在停頓裡 sweep 和 finalize 完；heap drop 時取消 sweep 的 job。
- `crates/elise/tests/it/concurrent.rs`（多數用測試自己的 platform：交出的 job 只在測試叫它時跑，mutator 在 marker 還沒走到某個物件時動手）：快照裡只經過一條邊到得了的物件，邊被剪掉、搬到新的物件上，barrier 留住它；marking 期間配置的是黑的（marker 沒看過也活著）；marking 期間的 scavenge 先 copy 再升上去，升上去的是黑的、它指的 old 物件由 barrier 留住，job 每次都重新交出；job 一直不跑時 old 撞到門檻加 margin、在停頓裡標完（margin 是門檻的一半，或比它大的 `min_overshoot_bytes`，到那之前安全點不收）；job 做完之後下一個安全點收尾、垃圾沒被標；在真的 pool 上一邊建 old 的鏈一邊剪掉舊的，marking 和 mutator 同時跑過、留下的都對。每個 heap 開著 verifier。只在 Miri 下編的：16 MiB 的 cage 上，預設的 margin 最多是 cage 剩下的一半。
- `crates/elise/tests/it/weak.rs`（不做 concurrent marking 的 heap，marking 的兩個用 `concurrent.rs` 的 platform）：full GC 清掉指向死掉的 old 物件的 weak 邊（單一的和一排的）、留下活的；scavenge 清掉 young 的 holder 和 old 的 holder（經過 weak 的 slot set）指向死掉的年輕物件的 weak 邊，活的改到 copy，升上去之後換 full GC 判斷；holder 被 scavenge 升上去時它的 weak 邊記進 weak 的 slot set、升上去之後經過 barrier 寫的也是；weak root 跟著搬、死了清掉；marking 期間 weak 讀出的物件被標上（接到一個黑的物件上，收尾後還在）；concurrent marking 清掉指向快照裡死掉的 weak 邊，黑的物件的 weak 邊下一輪才清。
- `crates/elise/tests/it/js.rs`（測試自己的 engine：從自己的 thread 交出物件的位址、在 JS 的邊裡寫自己的值、收下 marker 交的邊）：driven 的 heap 裡帶 JS 的邊的物件一出生就在 old、別的照樣在 nursery，不是 driven 的 heap 都在 nursery；engine 推進的 marking 留下交過來的和它指著的、放掉沒交過來的；marking 期間新建的物件活過那一輪、下一輪沒交就放掉；marking 期間從 engine 讀回、接到掃過的物件上、沒交過來的物件被標上；scavenge 留下只有 engine 拿著的物件和它指著的年輕物件；marker 在背景和收尾時把標到的物件的 JS 的邊交給 engine、死的不交；driven 的 heap 配置再多也不自己開始 GC（`should_collect` 也不看門檻），nursery 滿了照樣自己 scavenge，告訴 engine 的大小含 nursery 的物件；marking 期間安全點把 barrier 的物件發布給 worker（停頓裡不用再標），engine 的每一步叫醒 worker；marking 還沒做完時 `finish_marking` panic；driven 的 marking 結束時 heap 用的是存活量（走訪的、marking 期間配置的和升級的），和之後的 sweep 算出的一樣；driven 的 heap 不能 `collect`、不是 driven 的 heap 不給 engine 位址、沒有 JS 的邊的型別不給；在真的 pool 上，一片樹，engine 從另一個 thread 交出一半的頂端，中間穿插 scavenge：交過的整棵留下、其他的放掉，engine 收到每個留下的節點的 JS 的邊一次；加上排程那幾個（步驟、時間、worker 的進度、ephemeron、trace；step 的上限在標完最後一個物件時剛好用完，key 標過的 ephemeron 的 value 照樣交給 worklist，marking 還沒做完、pause 不標它們）；沒被驅動的 heap 裡 JS 的邊是 embedder 的 word（一開始是 0、跟著 scavenge 搬、full GC 之後還在），driven 的 heap 裡讀寫它都 panic。
- `crates/elise/tests/it/ids.rs`：full GC 清掉死的 id、留下活的，id 不重複、沒發過的找不到；死光的塊放掉（`HeapStats::id_chunks`）；scavenge 搬動和清掉 young 的、升上去之後換 full GC 判斷；型別不符找不到、`Erased` 找得到；marking 期間找到的物件被標上。
- `crates/elise/tests/it/ephemeron.rs`（不做 concurrent marking 的 heap，marking 的用 `concurrent.rs` 的 platform）：full GC 留下活的 key 的 value，清掉死掉的 key 的 pair（value 指回 key 也收得掉）；兩個 pair 串成的鏈照反的順序找到也標完；scavenge 搬活的 pair、清掉 key 死掉的（value 多活一次）、old 的 holder 經過 ephemeron 的 slot set；concurrent marking 留下活的、清掉死的、marking 期間讀出的 key 留下 pair；marking 期間只讀 value 時 key 被標上、value 留下；marking 期間被覆寫的 value 由 SATB 留下。
- `crates/elise/tests/it/parallel.rs`：在 3 個 worker 的 pool 上標一棵 5 萬個節點、有垃圾的樹，留下和釋放的都和只在呼叫的 thread 上標的一樣；寬的圖 worker 有幫忙（五次 GC 裡至少一次）；一條鏈從頭到尾不叫醒 worker。
- `crates/elise/tests/it/shuttle.rs`（只在 `--cfg shuttle`）：2 個 worker 的 pool 上標一棵 300 個節點、有垃圾的樹，隨機排程 2 萬次，每次留下和釋放的都對、site 標到的數目對，而且有排程讓 worker 真的標到東西；同樣的 pool 上 scavenge 被 10 頁 old 的 holder 和一個 root 指著的 40 個年輕物件，隨機排程 1000 次，每次留下、搬到的、finalize 的都對，而且有排程讓 worker 處理 slot set 的頁、讓兩個 thread 搶同一個物件；concurrent marking 和 mutator 同時跑：worker 在背景標、mutator 一邊改邊（剪掉舊的、接上新配置的）一邊刪 root、經過 weak root 讀出一個只被 weak 指著的物件接回 list，隨機排程，每次留下的都和只有一個 thread 時一樣，指向剪掉的物件的 weak root 被清掉；sweep 和 mutator 同時跑：worker 在背景 sweep 好幾頁（和 finalize `External` 的垃圾），mutator 一邊配置（沒有掃好的頁時自己掃一頁）一邊 scavenge 升上去，隨機排程 1000 次，兩串 list 都完整、垃圾每個 finalize 一次，而且有排程讓 worker 的 sweep、`Drop` 和 mutator 的操作重疊。
- `crates/elise/src/heap/young.rs` 的 loom 模型（只在 `--cfg loom`）：三個 thread 同時 `forward` 一個物件的 header，每一種 interleaving 都只有一份 copy 贏、每個 thread 拿到的去向都是它、輸的分得出自己那份。
- `crates/elise/src/worklist.rs` 的 loom 模型（只在 `--cfg loom`）：呼叫的 thread 和兩個 worker、15 項的樹、一段兩項、最多搶占三次的每一種 interleaving：每項只看一次、呼叫的 thread 全部做完才返回、沒有 deadlock，而且至少有一種讓 worker 拿到東西、總數超過一萬種（模型被改小時失敗）；背景的 run 拿著最後一段時，「做完了」的判斷要等它返回。
- `crates/elise/src/heap/concurrent.rs`、`crates/elise/src/platform.rs` 的 loom 模型（只在 `--cfg loom`）：mutator 先設 mark bit、寫好物件、再 release 寫邊，marker relaxed 讀邊再 acquire 的 fence，每一種 interleaving 裡看到邊的 marker 都看得到它標過、寫好了；`cancel` 返回之後交出的 job 不再有 run。
- `crates/elise/tests/it/ops.rs`、`crates/elise/tests/it/fuzz.rs`：fuzz 的操作和參考模型（一張不回收的圖）。bytes 解成一串操作（配置物件（`Obj` 的 finalizer 在 heap 的 thread 上；`AllocSent` 另外掛一個只被它指著、finalizer 在 sweep 的 worker 上的 `Tag`）、blob、寫邊、`HeapVector` 的 push 和 remove、root 的增刪、安全點、強制 GC、scavenge，開頭的 byte 選 stress、pool、小的門檻和一頁的 nursery、不會長大的 nursery（site 第一次 scavenge 就 tenure）；物件可以從兩個 site 配置），每個操作開始時從 root 走一遍、逐欄位和模型比，模型到得了的不能被 finalize，最後每個有 finalizer 的剛好 finalize 一次；heap 開著 verifier。物件多一條 weak 的邊和一個 ephemeron，操作多了寫和讀 weak 的邊、加刪和讀 weak root、寫和讀 pair、給 id 和用 id 找（放在別的操作的空 bit 上，所以以前的 regression 解出來的操作不變；用 id 找的時候另一種型別找不到）：讀到的要和模型一樣，或在模型裡的目標已經可能被收掉（GC 之後 root 到不了）之後才是空的，讀到的物件和它到得了的就算活著（「可能被收掉」在安全點可能做 GC 之前就記下：同一個操作裡的讀取看得到那次 GC）；開頭的 byte 另一個 bit 選 concurrent marking：platform 是不開 thread 的、交出的 job 只在 `MarkStep` 的操作（照 bytes 給的預算做幾步）時跑，所以照樣可以重播，標記和 mutator 的操作交錯；`fuzz.rs` 重播 `crates/elise/fuzz/regressions/` 的每個檔案、跑 40 段亂數的 bytes、兩段寫好的操作（第二段讓一個 site tenure 再撤回），和編碼的來回。
- **fuzz**（`crates/elise/fuzz`，cargo-fuzz，自己的 workspace，`rust-toolchain.toml` 釘 Miri 也用的 nightly）：target 用 `#[path]` 引同一個 `ops.rs`。`cfg(fuzzing)` 下 sweep 把空的格子和頁對 ASan 毒化、配置時解開，讀到死物件的那一刻 ASan 就停下。`test.py fuzz --seeds <目錄>` 另外從那個目錄的輸入開始（kun 的 `gc_bench fuzz-seeds` 從它的 heap 的錄製做種子：每份取開頭和第 100 萬、1000 萬、5000 萬筆記錄起的 2000 筆，換成這裡的操作），corpus 在 `target/fuzz/corpus/` 接著用；失敗時縮小輸入、印出路徑，放進 `crates/elise/fuzz/regressions/` 和修正一起 commit。
- **Verus**：`test.py verus` 用 `cargo verus verify -p elise` 讓整個 crate 經過 Verus，證明 `verus!` 裡的規格（約 1 秒，第一次編約 1 分鐘）。Verus 的版本在 `test.py` 的 `VERUS_VERSION`，裝在 `~/verus/<版本>/`（或 `VERUS_DIR`）；`vstd` 的版本釘在 workspace 的 `Cargo.toml`，兩個一起升級。`verus!` 裡的程式只能用 Verus 認得的 Rust（沒有 `while let`，位移量用 `u64`），clippy 和 numerics 的規則照樣管它，`#[expect]` 的理由寫 Verus 證了什麼；只在驗證時存在的 import 用 `cfg(verus_only)`。
- `crates/elise/tests/compile_fail.rs`（trybuild，自己一個 binary）：`crates/elise/tests/ui/` 的每個檔案違反一條規則、必須編不過。升級 toolchain 後用 `TRYBUILD=overwrite cargo test -p elise --test compile_fail` 重新產生 `.stderr`，確認錯誤的種類和位置沒變。
- 單元測試：verifier 抓到弄壞的 heap（活物件沒標、垃圾被標、空區段蓋到活物件、邊指到釋放的頁、commit 的總數錯；scavenge 之後沒記進 slot set 的老邊、到得了的 `FORWARDED` 物件）；worklist 每項只看一次（一個 thread、四個 thread）、背景的 run 在最後一項被要求讓出時照樣算清空、一個 thread panic 時呼叫的 thread 不會一直等；platform 叫醒工作要的 worker、只在呼叫的 thread 上做、worker 的 panic 到呼叫的 thread，交出的 job 在 worker 上跑到 `cancel`（等它的 run 返回、之後不再跑）、`run` 開始時讓出 worker 再還給它、兩個 job 分 worker、它的 panic 到 `cancel`、handle drop 時取消；`Marks::set_range` 跨 word 整段設上和清掉；verifier 抓到 old 物件的錯的 flags 和不認得的 site、指向死物件的 weak 邊、沒記進 weak 的 slot set 的 old 指向 young 的 weak 邊、活的 key 的 value 沒標、沒有 key 的 value、指向死物件的 id、沒放掉的空塊、沒列成 young 的 young 的 id、列成 young 的 old 的 id、沒有活的物件的 slot set 記錄、不在快照也不是 marking 期間配置的標過的物件；size class 的間距和查表、乘法和除法對每個頁內的位移都一樣、從位址找格子、bitmap 的空區段、sweep 找出死的有 finalizer 的物件、cage 滿了 panic；照 V8 的挑法挑要搬空的頁（空 70% 以上才拿、活的少的先、最多搬 4 MiB、放不掉頁就一頁都不拿、每一級各自擠）。

## TLA+ 的模型

```sh
python3 tla/marking/check.py [--spec Satb|SatbYoung|Dijkstra]   # concurrent marking 的協定
python3 tla/unified/check.py [--mutation NAME]                  # 和 JS engine 一起標記
```

`tla/marking/` 是 concurrent marking 的協定：`Satb.tla`（SATB 和 black allocation）、`SatbYoung.tla`（實作的協定，加上 young generation、weak 和 ephemeron）、`Dijkstra.tla`（V8 的 Dijkstra barrier，對照用）。`tla/unified/Unified.tla` 是 elise 的 marking 和 V8 的在同一次 major GC 裡一起做（I11）。每個 `check.py` 用 TLC 檢查模型本身成立，再拿掉每一部分（`Mutation`）確認 TLC 找得到它列出的反例。TLC 從 TLA+ tools 的 release 下載（版本和 SHA-256 釘在 `check.py`，放在 `target/tla/`），要有 Java（`PATH` 上的 `java`、`JAVA_HOME`，或 `--java`）。改到 barrier、black allocation、marking 的開始和收尾、scavenge 和 marking 的交錯、weak 或 ephemeron 的處理，或和 engine 之間的協定時，先改模型再改程式。
