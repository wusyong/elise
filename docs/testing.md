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
- `crates/elise/tests/it/alloc.rs`：沒有 nursery 的 heap 的快路：同一級的物件只在換區段時走慢路（`slow_allocations` 等於 `old_ranges`），`HeapVector` 也是；`used_bytes` 是物件的格子、不含區段的其餘；不看 site（有 nursery 的照樣數）；有 drop glue 的物件死了 finalize 一次，`LocalExternal` 的在 heap 的 thread 上，sweep 在 worker 上跑時也是（區段由沒有 drop glue 的物件拿到，頁上的計數全從快路來）。
- `crates/elise/tests/it/young.rs`（另外：nursery 剛好滿到 vector 本身配置在 old 時，它指向 young 的 store 也記下了；nursery 隨活下來的長大、不超過上限，垃圾之後 full GC 減半、不小於一開始，full GC 自己的 scavenge 不算，縮小之後累積重新算起；`pretenured` 的物件一開始就是 old、它指向 young 的邊有記下，closure panic 之後回到 nursery）：scavenge 只 copy 活的（物件搬了、值不變、一樣大）、死的 finalizer 跑一次；第二次活下來才升到 old、之後不再搬；old 物件經過 `Member::set`、`HeapVector` 的 push、set、remove、配置時的 init（large object、nursery 滿了之後配置在 old 的）指向 young 的邊留住它，清掉之後放掉；nursery 滿了之後下一個安全點做 scavenge；full GC 清空 nursery；heap drop 時 nursery 的 finalizer 跑一次；`alloc_beside` 配在擁有者的那一代（old 的擁有者旁邊直接放 old、不搬，young 的旁邊放 nursery），`HeapVector::new_beside` 的 vector 和 store 跟著擁有者，old 的 vector 長大時新的 store 也是 old。
- `crates/elise/tests/it/walk.rs`：old 的（full GC 留下的和之後配置的）和 nursery 的物件都在、每個一次，垃圾到 full GC 之後才不在；物件的型別、大小、照欄位名字的邊（strong、weak、ephemeron 的 key 和 value、`HeapVector` 的 store 和沒有名字的陣列）、JS 的邊和 root；large object 和正在 bump 的頁；GC 放掉的格子和 cursor 還沒 bump 到的那段（header 還寫著死掉的物件的型別）沒有物件；concurrent marking 之後 sweep 還沒跑時先收完 sweep（垃圾不在），process 共用的 pool 上的也是；marking 期間交出的垃圾接到活的物件上之後活過那一輪。
- `crates/elise/tests/it/sites.rs`：活下來的 site 在 nursery 夠大時 tenure、死的不 tenure，兩個 site 分開算；小的 nursery 先記成「可能」，長大之後才 tenure；只有第一次活下來的算（老的升上去不算）；full GC 撤回 old 的物件死光的 site、留下活著的；`pretenured` 比 site 優先；物件的 site 經過 copy 和升級還在（標記時各算各的）。heap 在 process 共用的 pool 上 scavenge 和標記，thread 的計數合起來要對；每個 heap 開著 verifier（它檢查每個物件的 flags 和 site）。`crates/elise/src/site.rs` 的單元測試：決定的門檻、100 個以下不判斷、「可能」的轉換、撤回的計算、編號用完之後是 0。
- `crates/elise/tests/it/fragmentation.rs`（沒有 nursery 的 heap）：頁上一成的格子活著時照 V8 的挑法全部搬空、擠到一頁、放掉三頁，一半或全部活著時不搬；頁的格子、空的和 `used_bytes` 的帳對得上；每次 sweep 做完和每次 marking 開始各量一次、沒打開時不量；marking 開始時量到 sweep 之後配置填掉的空格子（垃圾填滿的頁那時不搬，sweep 之後又一樣空）；配置在隔一格一個活物件的頁上一次拿一格的區段，新的頁一次拿整頁。
- `crates/elise/tests/it/sweep.rs`（測試自己的 platform：sweep 的 job 只在測試叫它時跑）：marking 收尾之後頁等著 sweep，`used` 在 sweep 做完之前含還沒掃的垃圾、做完是 sweep 找到的活的量（也是 GC 的 marked bytes），配置時那一級沒有掃好的頁就自己掃一頁（只掃那一級的，目前的頁的空區段用完才掃）；job 掃完之後下一個安全點（`scavenge` 的也是）套用、sweep 結束（碎片在這時才量，concurrent marking 開始時也量，heap 的 thread 正在 bump 的那段算空的，nursery 的頁不算 old 的 committed），之後配置不再自己掃，先拿 sweep 找到空格子的頁；下一輪 marking 開始和 `collect` 在停頓裡掃完剩下的；sweep 期間的 scavenge 不升到還沒掃的頁上；有 `LocalExternal` 的物件（和 large object）不在收尾的停頓裡 finalize，下一個安全點才 finalize，同一頁上 `Send` 的也一起；`External` 的物件（和 large object）由 job finalize，配置同一級時不拿那一頁，large object 的頁之後被重用；job 上的 finalizer panic 時往上拋、那一頁之後重掃、不 finalize 兩次；真的 pool 上 `External` 的 `Drop` 在 worker 上跑；`collect` 在停頓裡 sweep 和 finalize 完；heap drop 時取消 sweep 的 job。
- `crates/elise/tests/it/concurrent.rs`（多數用測試自己的 platform：交出的 job 只在測試叫它時跑，mutator 在 marker 還沒走到某個物件時動手）：快照裡只經過一條邊到得了的物件，邊被剪掉、搬到新的物件上，barrier 留住它；marking 期間配置的是黑的（marker 沒看過也活著）；marking 期間的 scavenge 先 copy 再升上去，升上去的是黑的、它指的 old 物件由 barrier 留住，job 每次都重新交出；job 一直不跑時 old 撞到門檻加 margin、在停頓裡標完（margin 是門檻的一半，或比它大的 `min_overshoot_bytes`，到那之前安全點不收）；job 做完之後下一個安全點收尾、垃圾沒被標；在真的 pool 上一邊建 old 的鏈一邊剪掉舊的，marking 和 mutator 同時跑過、留下的都對。每個 heap 開著 verifier。只在 Miri 下編的：16 MiB 的 cage 上，預設的 margin 最多是 cage 剩下的一半。
- `crates/elise/tests/it/weak.rs`（不做 concurrent marking 的 heap，marking 的兩個用 `concurrent.rs` 的 platform）：full GC 清掉指向死掉的 old 物件的 weak 邊（單一的和一排的）、留下活的；scavenge 清掉 young 的 holder 和 old 的 holder（經過 weak 的 slot set）指向死掉的年輕物件的 weak 邊，活的改到 copy，升上去之後換 full GC 判斷；holder 被 scavenge 升上去時它的 weak 邊記進 weak 的 slot set、升上去之後經過 barrier 寫的也是；weak root 跟著搬、死了清掉；marking 期間 weak 讀出的物件被標上（接到一個黑的物件上，收尾後還在）；concurrent marking 清掉指向快照裡死掉的 weak 邊，黑的物件的 weak 邊下一輪才清。
- `crates/elise/tests/it/js.rs`（測試自己的 engine：從自己的 thread 交出物件的位址、在 JS 的邊裡寫自己的值、收下 marker 交的邊）：driven 的 heap 裡帶 JS 的邊的物件一出生就在 old、別的照樣在 nursery，不是 driven 的 heap 都在 nursery；engine 推進的 marking 留下交過來的和它指著的、放掉沒交過來的；marking 期間新建的物件活過那一輪、下一輪沒交就放掉；marking 期間從 engine 讀回、接到掃過的物件上、沒交過來的物件被標上；scavenge 留下只有 engine 拿著的物件和它指著的年輕物件；marker 在背景和收尾時把標到的物件的 JS 的邊交給 engine、死的不交；driven 的 heap 配置再多也不自己開始 GC（`should_collect` 也不看門檻），nursery 滿了照樣自己 scavenge，告訴 engine 的大小含 nursery 的物件；marking 期間安全點把 barrier 的物件發布給 worker（停頓裡不用再標），engine 的每一步叫醒 worker；marking 還沒做完時 `finish_marking` panic；driven 的 marking 結束時 heap 用的是存活量（走訪的、marking 期間配置的和升級的），和之後的 sweep 算出的一樣；driven 的 heap 不能 `collect`、不是 driven 的 heap 不給 engine 位址、沒有 JS 的邊的型別不給；在真的 pool 上，一片樹，engine 從另一個 thread 交出一半的頂端，中間穿插 scavenge：交過的整棵留下、其他的放掉，engine 收到每個留下的節點的 JS 的邊一次；加上排程那幾個（步驟、時間、worker 的進度、ephemeron、trace；step 的上限在標完最後一個物件時剛好用完，key 標過的 ephemeron 的 value 照樣交給 worklist，marking 還沒做完、pause 不標它們）；沒被驅動的 heap 裡 JS 的邊是 embedder 的 word（一開始是 0、跟著 scavenge 搬、full GC 之後還在），driven 的 heap 裡讀寫它都 panic。
- `crates/elise/tests/it/raw.rs`（執行時登記的型別，每個 heap 開著 verifier）：struct 的物件經過兩次 scavenge 和 full GC 留下它的邊到得了的、垃圾放掉；array 的邊留下它的元素（nursery 裡的小的、large object 的大的，old 的 array 指向 young 的元素經過 barrier）；資料的 array 被 scavenge 整個 copy（照長度算大小）；死物件的位置重新配置時是 0；Rust 的物件和執行時的物件互相指著，`downcast` 分得出來；old 的物件經過 `write_edge` 指向 young 的被 scavenge 留下，marking 期間被覆寫的舊值由 SATB 留下；layout 的每一種錯被拒絕、邊界上的接受；一個 id 登記一次、超出的拒絕；太大的 array 回 `None`；static 的、用錯 struct 和 array 的、退休的型別配置時 panic。`I31`：照 wasmtime 的 `I31` 轉換（截斷、放不下的拒絕、零擴展和符號擴展）；邊的 bits（none 是 0、物件是 4-aligned 的 offset、整數左移再設最低位元），寫進去的 bits 讀回來是它代表的值；struct 和 array 的邊裡的整數和物件並排，經過兩次 scavenge 和 full GC 不變，每個整數的 bits 都落在一個死物件裡面（scavenge 時是 young 的、full GC 時是 old 的：當成物件就會讀到不是 header 的地方，或留下死物件；full GC 標到的物件數剛好）；old 的物件的邊記進 slot set 之後被寫成落在死掉的 young 物件裡的整數，scavenge 經過 slot set 時跳過、什麼都不 copy；marking 期間被覆寫的舊值是落在死掉的 old 物件裡的整數，SATB 不標它。engine 的 frame：frame 裡的安全點不做任何 GC（nursery 滿了配置到 old、old 超過門檻也一樣），離開之後的下一個安全點做；`collect`、`scavenge` 在 frame 裡 panic，沒進去就離開也 panic；交出 root 的 scavenge 留下 root 到得了的、搬動之後寫回新的 bits，0 和 `I31` 的 root 不變；交出 root 的 full GC 只標 root 到得了的（標到的物件數剛好）；frame 裡的 marking 只在交出 root 時開始和收尾（worker 做完之後的安全點也不收），frame 從邊拿到、再剪掉那條邊的物件由 SATB 留下，frame 放掉的物件是 verifier 認得的 floating garbage；bits 的配置在 nursery 滿了之後回 `GcDue`，frame 裡的安全點不改變它，交出 root 的 GC 之後成功，`stress` 下 GC 之後的第一次成功、第二次又是 `GcDue`，太大的 array 先回 `TooLarge`；bits 寫的 old 物件指向 young 的邊經過 slot set 留下；第二次交出的 root 多了、少了都 panic。heap 之外的 root 的來源：`stress` 下每個安全點的 scavenge 和 concurrent marking 的開始、收尾都問它兩次（讀、寫回），它拿著的物件搬動之後寫回新的 bits、經過 marking 還在，0 和 `I31` 不變，旁邊的垃圾死掉；`collect`、`scavenge` 也問它，拿掉之後它拿著的物件死在下一次 full GC；它給不了時安全點不做 GC（nursery 滿了配置到 old、old 超過門檻），`collect`、`scavenge` 什麼都不做，給得了之後的下一個安全點做、它的物件還在；交出 frame 的 root 的 scavenge 和 full GC 讀它的 root、兩邊都寫回；它給不了時交出 root 的 GC 不做，之後的 bits 的配置成功；第二次給不了、給的少了都 panic。`Heap` 上的 engine 函式：物件的資料在 cage 的開頭加 bits，`size_at` 只從物件讀出大小（struct、array），id 在物件活著時活著、死在沒有 root 的 full GC，cage 經過 GC 不動；old 物件的邊不經過 barrier 寫成 young 的、`remember_in` 之後經過 scavenge 留下；marking 期間不經過 barrier 剪掉的邊的舊值交給 `satb_in` 之後留下（標到的物件數剛好），barrier state 的旗標跟著 marking；`HeapAlive` 只認它的 heap，在 heap drop 時清掉，在 frame 裡 drop 的 heap 的物件還讀得到。`crates/elise/tests/it/engine.rs`（`raw::EngineHeap`，開著 verifier）：它是 `Send` 和 `Sync`；搬到另一個 thread 上做交出 root 的 full GC 之後，物件和它的邊都還在；四個 thread 同時讀邊、資料和大小；struct 和兩種 array 的大小照 layout、array 的長度在它的位置，用量至少是配置的；交出 root 的 scavenge 搬動 root 拿著的物件、寫回新的 bits、放掉垃圾（用量變小），之後寫的邊經過 full GC 留下；scavenge 不放 old 的垃圾、full GC 放掉（用量）；id 在物件活著時活著：young 的物件死在 scavenge、它的 id 跟著死，升到 old 的經過 scavenge 還活著、死在 full GC；`stress` 下每個安全點都 GC、每一步都問 id 活著沒有，沒有 root 的物件還是在一次 marking 收尾時死掉（問不會標它）；`stress` 下配置回 `GcDue`、安全點之後成功；frame 的計數可以巢狀；升到 old 的物件經過 `write_bits` 指向 young 的邊被 scavenge 留下；太大的 array 回 `TooLarge`。barrier state：經過它的指標讀到的 marking 旗標跟著 marking 開始和收尾、young 頁的旗標跟著物件在不在 nursery，heap 換了 thread 之後位址不變；不經過 barrier 寫進 old 物件的指向 young 的邊，`remember` 之後經過 scavenge 留下；marking 期間不經過 barrier 剪掉的邊的舊值，`satb` 之後由 marking 留下（`I31` 的舊值什麼都不做）。退休的型別放掉的時機要整個 process 只有測試自己的 heap，所以這三個重新執行測試的 binary、只跑那一個（Miri 底下跳過）：marking 期間退休的型別，那次 GC 做完還不放、下一次做完才放，放掉之後那一格可以登記別的型別；兩個 heap 時，做完 GC 的那個 drop 了、另一個還沒做完也不放，另一個做完、或沒做完就 drop 才放；丟掉的一段編號，空的馬上還、退休的放掉之後還、登記著的不還；heap snapshot 的走訪找得到執行時的型別的物件、它們的大小（陣列照長度）和指向物件的邊（`I31` 和空的不算），有沒有 nursery 都一樣。`crates/elise/src/types.rs` 的單元測試在自己的 `Runtime` 上：每個 heap 都要做完一次、退休時已經開始的那次不算、drop 的 heap 和之後才建的不用等、沒有 heap 時馬上放掉、放掉之前那一格不能登記，編號的保留和歸還（第一個放得下的、合併、還到最後時退回去），丟掉的一段只還空的和放掉的。
- `crates/elise/tests/it/ids.rs`：full GC 清掉死的 id、留下活的，id 不重複、沒發過的找不到；死光的塊放掉（`HeapStats::id_chunks`）；scavenge 搬動和清掉 young 的、升上去之後換 full GC 判斷；型別不符找不到、`Erased` 找得到；marking 期間找到的物件被標上。
- `crates/elise/tests/it/ephemeron.rs`（不做 concurrent marking 的 heap，marking 的用 `concurrent.rs` 的 platform）：full GC 留下活的 key 的 value，清掉死掉的 key 的 pair（value 指回 key 也收得掉）；兩個 pair 串成的鏈照反的順序找到也標完；scavenge 搬活的 pair、清掉 key 死掉的（value 多活一次）、old 的 holder 經過 ephemeron 的 slot set；concurrent marking 留下活的、清掉死的、marking 期間讀出的 key 留下 pair；marking 期間只讀 value 時 key 被標上、value 留下；marking 期間被覆寫的 value 由 SATB 留下。
- `crates/elise/tests/it/parallel.rs`：在 3 個 worker 的 pool 上標一棵 5 萬個節點、有垃圾的樹，留下和釋放的都和只在呼叫的 thread 上標的一樣；寬的圖 worker 有幫忙（五次 GC 裡至少一次）；一條鏈從頭到尾不叫醒 worker。
- `crates/elise/tests/it/shuttle.rs`（只在 `--cfg shuttle`）：2 個 worker 的 pool 上標一棵 300 個節點、有垃圾的樹，隨機排程 2 萬次，每次留下和釋放的都對、site 標到的數目對，而且有排程讓 worker 真的標到東西；同樣的 pool 上 scavenge 被 10 頁 old 的 holder 和一個 root 指著的 40 個年輕物件，隨機排程 1000 次，每次留下、搬到的、finalize 的都對，而且有排程讓 worker 處理 slot set 的頁、讓兩個 thread 搶同一個物件；concurrent marking 和 mutator 同時跑：worker 在背景標、mutator 一邊改邊（剪掉舊的、接上新配置的）一邊刪 root、經過 weak root 讀出一個只被 weak 指著的物件接回 list，隨機排程，每次留下的都和只有一個 thread 時一樣，指向剪掉的物件的 weak root 被清掉；sweep 和 mutator 同時跑：worker 在背景 sweep 好幾頁（和 finalize `External` 的垃圾），mutator 一邊配置（沒有掃好的頁時自己掃一頁）一邊 scavenge 升上去，隨機排程 1000 次，兩串 list 都完整、垃圾每個 finalize 一次，而且有排程讓 worker 的 sweep、`Drop` 和 mutator 的操作重疊。
- `crates/elise/src/heap/young.rs` 的 loom 模型（只在 `--cfg loom`）：三個 thread 同時 `forward` 一個物件的 header，每一種 interleaving 都只有一份 copy 贏、每個 thread 拿到的去向都是它、輸的分得出自己那份。
- `crates/elise/src/worklist.rs` 的 loom 模型（只在 `--cfg loom`）：呼叫的 thread 和兩個 worker、15 項的樹、一段兩項、最多搶占三次的每一種 interleaving：每項只看一次、呼叫的 thread 全部做完才返回、沒有 deadlock，而且至少有一種讓 worker 拿到東西、總數超過一萬種（模型被改小時失敗）；背景的 run 拿著最後一段時，「做完了」的判斷要等它返回。
- `crates/elise/src/heap/concurrent.rs`、`crates/elise/src/platform.rs` 的 loom 模型（只在 `--cfg loom`）：mutator 先設 mark bit、寫好物件、再 release 寫邊，marker relaxed 讀邊再 acquire 的 fence，每一種 interleaving 裡看到邊的 marker 都看得到它標過、寫好了；`cancel` 返回之後交出的 job 不再有 run。
- `crates/elise/tests/it/ops.rs`、`crates/elise/tests/it/fuzz.rs`：fuzz 的操作和參考模型（一張不回收的圖）。bytes 解成一串操作（配置物件（`Obj` 的 finalizer 在 heap 的 thread 上；`AllocSent` 另外掛一個只被它指著、finalizer 在 sweep 的 worker 上的 `Tag`）、blob、寫邊、`HeapVector` 的 push 和 remove、root 的增刪、安全點、強制 GC、scavenge，開頭的 byte 選 stress、pool、小的門檻和一頁的 nursery、不會長大的 nursery（site 第一次 scavenge 就 tenure）；物件可以從兩個 site 配置），每個操作開始時從 root 走一遍、逐欄位和模型比，模型到得了的不能被 finalize，最後每個有 finalizer 的剛好 finalize 一次；heap 開著 verifier。物件多一條 weak 的邊和一個 ephemeron，操作多了寫和讀 weak 的邊、加刪和讀 weak root、寫和讀 pair、給 id 和用 id 找（放在別的操作的空 bit 上，所以以前的 regression 解出來的操作不變；用 id 找的時候另一種型別找不到）：讀到的要和模型一樣，或在模型裡的目標已經可能被收掉（GC 之後 root 到不了）之後才是空的，讀到的物件和它到得了的就算活著（「可能被收掉」在安全點可能做 GC 之前就記下：同一個操作裡的讀取看得到那次 GC）；開頭的 byte 另一個 bit 選 concurrent marking：platform 是不開 thread 的、交出的 job 只在 `MarkStep` 的操作（照 bytes 給的預算做幾步）時跑，所以照樣可以重播，標記和 mutator 的操作交錯；`fuzz.rs` 重播 `crates/elise/fuzz/regressions/` 的每個檔案、跑 40 段亂數的 bytes、兩段寫好的操作（第二段讓一個 site tenure 再撤回），和編碼的來回。
- **fuzz**（`crates/elise/fuzz`，cargo-fuzz，自己的 workspace，`rust-toolchain.toml` 釘 Miri 也用的 nightly）：target 用 `#[path]` 引同一個 `ops.rs`。`cfg(fuzzing)` 下 sweep 把空的格子和頁對 ASan 毒化、配置時解開，讀到死物件的那一刻 ASan 就停下。`test.py fuzz --seeds <目錄>` 另外從那個目錄的輸入開始（machopo 的 `gc_bench fuzz-seeds` 從它的 heap 的錄製做種子：每份取開頭和第 100 萬、1000 萬、5000 萬筆記錄起的 2000 筆，換成這裡的操作），corpus 在 `target/fuzz/corpus/` 接著用；失敗時縮小輸入、印出路徑，放進 `crates/elise/fuzz/regressions/` 和修正一起 commit。
- **Verus**：`test.py verus` 用 `cargo verus verify -p elise` 讓整個 crate 經過 Verus，證明 `verus!` 裡的規格（約 1 秒，第一次編約 1 分鐘）。Verus 的版本在 `test.py` 的 `VERUS_VERSION`，裝在 `~/verus/<版本>/`（或 `VERUS_DIR`）；`vstd` 的版本釘在 workspace 的 `Cargo.toml`，兩個一起升級。`verus!` 裡的程式只能用 Verus 認得的 Rust（沒有 `while let`，位移量用 `u64`），clippy 和 numerics 的規則照樣管它，`#[expect]` 的理由寫 Verus 證了什麼；只在驗證時存在的 import 用 `cfg(verus_only)`。
- `crates/elise/tests/compile_fail.rs`（trybuild，自己一個 binary）：`crates/elise/tests/ui/` 的每個檔案違反一條規則、必須編不過。升級 toolchain 後用 `TRYBUILD=overwrite cargo test -p elise --test compile_fail` 重新產生 `.stderr`，確認錯誤的種類和位置沒變。
- 單元測試：verifier 抓到弄壞的 heap（活物件沒標、垃圾被標、空區段蓋到活物件、邊指到釋放的頁、commit 的總數錯；scavenge 之後沒記進 slot set 的老邊、到得了的 `FORWARDED` 物件）；worklist 每項只看一次（一個 thread、四個 thread）、背景的 run 在最後一項被要求讓出時照樣算清空、一個 thread panic 時呼叫的 thread 不會一直等；platform 叫醒工作要的 worker、只在呼叫的 thread 上做、worker 的 panic 到呼叫的 thread，交出的 job 在 worker 上跑到 `cancel`（等它的 run 返回、之後不再跑）、`run` 開始時讓出 worker 再還給它、兩個 job 分 worker、它的 panic 到 `cancel`、handle drop 時取消；`Marks::set_range` 跨 word 整段設上和清掉；verifier 抓到 old 物件的錯的 flags 和不認得的 site、指向死物件的 weak 邊、沒記進 weak 的 slot set 的 old 指向 young 的 weak 邊、活的 key 的 value 沒標、沒有 key 的 value、指向死物件的 id、沒放掉的空塊、沒列成 young 的 young 的 id、列成 young 的 old 的 id、沒有活的物件的 slot set 記錄、不在快照也不是 marking 期間配置的標過的物件、只有執行時的型別的邊到得了的物件沒標、執行時的型別的 old 物件指向 young 的邊沒記進 slot set；verifier 從交出的 frame 的 root 走（只有它們到得了的 old 物件不算走不到，young 物件照樣檢查）；寫進執行時的型別的 old 物件、bits 落在 young 的頁上的 `I31` 不記進 slot set（同一格寫 young 物件的 offset 才記）；size class 的間距和查表、乘法和除法對每個頁內的位移都一樣、從位址找格子、bitmap 的空區段、sweep 找出死的有 finalizer 的物件、cage 滿了 panic；照 V8 的挑法挑要搬空的頁（空 70% 以上才拿、活的少的先、最多搬 4 MiB、放不掉頁就一頁都不拿、每一級各自擠）。

## TLA+ 的模型

```sh
python3 tla/marking/check.py [--spec Satb|SatbYoung|Dijkstra]   # concurrent marking 的協定
python3 tla/unified/check.py [--mutation NAME]                  # 和 JS engine 一起標記
```

`tla/marking/` 是 concurrent marking 的協定：`Satb.tla`（SATB 和 black allocation）、`SatbYoung.tla`（實作的協定，加上 young generation、weak 和 ephemeron）、`Dijkstra.tla`（V8 的 Dijkstra barrier，對照用）。`tla/unified/Unified.tla` 是 elise 的 marking 和 V8 的在同一次 major GC 裡一起做（I11）。每個 `check.py` 用 TLC 檢查模型本身成立，再拿掉每一部分（`Mutation`）確認 TLC 找得到它列出的反例。TLC 從 TLA+ tools 的 release 下載（版本和 SHA-256 釘在 `check.py`，放在 `target/tla/`），要有 Java（`PATH` 上的 `java`、`JAVA_HOME`，或 `--java`）。改到 barrier、black allocation、marking 的開始和收尾、scavenge 和 marking 的交錯、weak 或 ephemeron 的處理，或和 engine 之間的協定時，先改模型再改程式。

## 測試守不住的地方

測試沒有守住、只靠審查或之後才補的地方。改到它們時照這裡的理由看一次。

### 只靠審查

| 項目 | 為什麼測不到 | 現在靠什麼 | 之後怎麼處理 |
| --- | --- | --- | --- |
| `Marker::retire` 等進行中的交接做完才返回（engine 的 thread 正在 `hand` 裡、已經看過 marker 開著，heap 收尾或丟掉要等它設完 mark bit） | 漏掉的只是一個交錯：交接看過開著之後、設 mark bit 之前，heap 就收尾並釋放了頁；後果是寫到釋放的記憶體，測試看不到（拿掉等待的 mutation 沒有測試失敗） | 審查：交接先加計數再看開著沒，`retire` 先關再等計數歸零，都是 SeqCst（`soundness.md` 的 I11） | 用 shuttle 排出這個交錯，讓 heap 在 marker 的 mark table 還有交接時改用會檢查的 debug 版（例如 drop 時把它毒化），或在 Miri 底下跑這個交錯 |
| slot set 第一次配置 bitmap 時，CAS 輸的 thread 丟掉自己的、用贏家的（`page::SlotSet::insert`） | 兩個 scavenge 的 thread 同時第一次記同一頁才會碰到；slot set 用 std 的 atomic（一張每頁一格的表，每個 heap 一萬多格，換成 shuttle 的 atomic 會讓每個排程都建一萬多個），shuttle 不在它上面切換 thread，loom 的模型只有 forwarding | 審查 | 把 `SlotSet` 寫成對 atomic 泛型（像 `forward` 的 `HeaderWord`），加一個 loom 的模型：兩個 thread 同時記同一頁的不同格，兩格都在 |
| concurrent marking 的發布順序：marking 中寫邊和 weak 的邊用 release（`Inner::write_edge`、`write_weak`）、背景的 marker 讀一批邊（和 weak 的邊）之後的 acquire fence（`Marker::edges`）、頁進 `MarkTable` 用 release（`MarkTable::insert`） | 換成 relaxed 或拿掉 fence，要 marker 正好在 mutator 那幾個指令之間讀到邊、CPU 又真的重排了才會出錯，測試排不出來；heap 的邊是 cage 裡的 std 的 atomic，loom 管不到 | loom 的模型（同樣的順序的複本：換成 relaxed 或拿掉 fence 時找得到 marker 看到沒標的物件）、審查 | 把寫邊和讀一批邊的順序寫成對 atomic 泛型的小函式（像 scavenge 的 `forward` 的 `HeaderWord`），loom 的模型呼叫同一段 |
| `HeapData` impl 和 derive 產生的 layout（G1、G2）、測試沒走到的 `unsafe` 路徑、cage 滿了的路徑 | `HeapData` 錯了（型別其實有指標或 drop glue）不是當下的 UB；Miri 只檢查測試走到的路徑；填滿 4 GiB 的 cage 太慢 | `soundness.md` 的「需要人工審查的地方」、compile-fail 測試、mutation test、測試在 Miri 下跑；cage 滿了由單元測試把下一頁設成最後一頁 | fuzz（`scripts/test.py fuzz`，錄製當種子、對著參考模型）涵蓋更多路徑，找到的 regression 在 Miri 下重播；頁的 bitmap 搜尋已經由 Verus 證明，之後把壓縮指標、配置的帳也放進 `verus!` |
| marking 期間的 overshoot margin 不超過 cage 在門檻之外剩下的一半，在 marking 開始時算好、到收尾不變（`Inner::overshoot_margin`，照 V8 的 `old_generation_overshoot_margin`） | 要碰到這個上限，old 要先長到 cage（4 GiB）的一半左右，一般的 build 不跑；Miri 下的 cage 只有 16 MiB，預設的 32 MiB 的 margin 才會碰到它，只在 Miri 下編的 `concurrent::the_margin_is_at_most_half_of_the_cage_left` 測它（mutation test 用 Miri 跑過：上限放寬、跟著 old 長大重算都抓到），但 Miri 每週才跑一次，`cargo test` 抓不到 | CI 每週的 Miri、審查；margin 的其他部分（門檻的一半、`min_overshoot_bytes`、沒在 marking 時不加）由 `concurrent::` 的測試和 mutation test 守住 | 讓 cage 的大小可以在測試裡縮小，一般的 build 也測得到 |
| 新的 GC 的頁的 `Marks` 不放在 `Box` 裡（`page::OwnedMarks`）：`MarkTable` 另外指向它，`Box` 每次跟著頁搬都在 Stacked Borrows 下重新宣稱唯一，那個指標就失效 | 一般的 build 不出事（`Box` 的 `noalias` 只用在函式參數上），只有 Miri 看得出來；Miri 每週才跑一次，改回 `Box` 的 mutation test 要跑 Miri，沒做 | CI 每週的 Miri（收尾時它找到原本的寫法）、`soundness.md` 的論證 | 審查：不要把 `OwnedMarks` 換回 `Box`，其他被 `MarkTable` 這類表另外指著的配置也一樣 |
| 新的 GC 真的把空頁的記憶體還給作業系統（`Cage::discard` 和 worker 上的 `Discarder::discard`），和 verifier 對 worker 正在還的那一頁的判斷（`Discards` 的 `busy`） | 還掉的頁之後會重新 commit，讀到的是 0，但配置都會寫滿物件，測試看不出記憶體有沒有還；要看作業系統的 resident（`mincore`、task 的統計），測試不碰。`busy` 只在 worker 和 verifier 同時跑的那一刻有差（漏了它，verifier 偶爾誤報：做的時候在 pool 上的測試看到過） | 審查；heap 這一側的帳（頁的狀態、committed、拿回來的時機）由 `heap::` 的測試和 mutation test 守住 | machopo 的 `gc_bench` 的報告有 resident，之後在它的測試裡檢查 GC 之後 resident 降下來 |

### 沒有計數守著的優化

拿掉之後結果一樣、只是變慢或多用記憶體，測試抓不到；每一條寫了要數什麼才守得住。

- **marking 發布一段時叫醒 worker**（`Platform`，呼叫的 thread 的 `notify_concurrency_increase`）：不叫醒時結果一樣，只是 worker 只在 `run` 一開始被叫醒、剛好看到已經發布的段時才幫忙，幾乎變成單 thread。`workers_help_with_a_wide_graph` 只要五次 GC 裡有一次 worker 幫忙就過，抓不到。要數：每次 GC 有多少段被 worker 拿走，或者 worker 被叫醒的次數。
- **新的 GC 的平行 marking 怎麼分工**（`worklist`、`ThreadPool`）：一段 512 項（cppgc 的大小）、發布一段只叫醒一個 worker、開工時沒有段可以拿就不叫醒。改回一段 64 項、`notify_all`、開工時一律叫醒，結果一樣，只是窄的圖（machopo 的 `gc_bench` 的 tree：一串 128 個物件的小樹）被切給 worker，拿的成本比標的多，比單 thread 慢 5 倍，小的圖（replace，一千個物件）多 3 倍的時間（2026-10-25，Apple M4）。反過來，工作一開始就要 worker 時一定要叫醒（不然呼叫的 thread 一直等），這由 `platform` 的單元測試守住（worker 都睡下之後才開工，等不到就失敗）。要數：每次 GC 發布了幾段、worker 被叫醒的次數；或在 machopo 的 `bench.py gc` 檢查 gc-parallel 的 marking 不比 gc 慢。
- **新的 GC 的平行 scavenge 怎麼省**（`heap/young.rs`）：slot set 的頁湊滿一段才給 worker（不然 nursery 小時每次都叫醒一個 worker 做一點點，2026-10-26 量到 replace 慢 18%）；worker 結束時把它每一級沒用完的區段還回頁上（不還，那些格子要等下一次 full GC 的 sweep 才重新找到）；搶輸的 copy 在最上面時退回。拿掉任何一個，結果照樣對，只是慢或多用記憶體。要數：每次 scavenge 叫醒的 worker、還回的區段的格子數；或在 machopo 的 `bench.py gc` 檢查 gc-parallel 的 scavenge 不比 gc 慢。（LAB 的最後一段退回由 `young.rs` 的測試守住：一個 thread 時 nursery 剛好是 copy 的大小。）
- **新的 GC 的配置自己 sweep 之前先套用 worker 掃好的結果**（`Inner::sweep_for`）：不先套用結果一樣，只是 heap 的 thread 多掃那一級還沒掃的頁，worker 已經掃好的要等下一個安全點才用到。測試的 platform 只在測試叫它時跑 job，安全點已經套用了它掃好的，配置時不會碰到「已經掃好、還沒套用」的頁，看不出差別。要守的話數 heap 的 thread 自己掃頁時，清單上已經有幾頁那一級的結果。
- **scavenge 只走用過的頁的 weak 和 ephemeron 的 slot set**（`heap/young.rs` 的 `update_weak`）：改成走整張表（16384 頁）結果一樣，只是每次 scavenge 多約 30 µs，存活率低的重播的 GC 時間變成 20 倍（2026-10-26 的量測抓到）。要守的話數 `update_weak` 看了幾頁的 slot set，應該不超過用過的頁數。
- **marker 只記下要清的 weak 格子**（`heap/concurrent.rs`）：只記目標是 old、掃的時候還沒標的（cppgc 的 `RegisterWeakReferenceIfNeeded` 同樣過濾），不記指向標過的和 young 的。拿掉過濾結果一樣（收尾時照樣再看一次目標，young 的不清），只是清單變長、收尾多看一些格子。要守的話數記下的格子，有 weak 欄位的物件指向活物件時應該是 0。
- **sweep 不碰沒有 finalizer 的死物件**：sweep 只讀頁旁邊的 bitmap，有 finalizer 的死物件才讀它。改成讀每個死物件的 header 結果一樣，只是碰到全部的垃圾。要數：sweep 讀了幾個物件（debug build 填毒值的不算）。
- **沒有 nursery 的 heap 不查 site 的編號**（`Mutation::alloc_in`、`alloc_beside` 在 `has_no_nursery` 時直接用 0）：照樣查時結果一樣（快路不看 site，header 裡 site 的編號也寫 0），只多一次 atomic 的讀（`Site::index`）。要守的話數 `Site::index` 的呼叫。

### 等價、不用處理的 mutation

弄壞之後行為完全一樣，記在這裡免得重複追查。

- scavenge 切 LAB 時，頁剩下的不夠就把頁尾填成空段（`heap/young.rs` 的 `Source::cut`）：LAB 一次 32 KiB、整除一頁，nursery 的物件最大 32 KiB，所以頁尾不會短，這段走不到；留著是為了 LAB 的大小改了時，young 的頁照樣從頭走得到 `top`（verifier 檢查）。
- marking 收尾再跑一次 `run` 時，`Worklist::reseed` 讓 joining 的 run 一開始就算 busy（`worklist.rs`）：不設時 joining 的 run 做完自己的可能提早把 `done` 設上，但拿著段的 worker 在自己的迴圈裡做完全域剩下的段，`Platform::run` 又等所有 worker 返回才返回，所以標的一樣。留著是為了和 `Worklist::new` 的不變式一致（joining 的 run 帶著 seed、一開始就算 busy）。
- 配置時不在 `marks` 設新物件的 bit（`Page::publish`；large object 的 `marked`）：只有 debug build 設，讓下一次 GC 知道它死了要填毒值；配置看的是 sweep 找出的空區段，marking 從清空的 bitmap 開始，沒有 bit 只是 debug 少填一些毒值。
- concurrent marking 不處理刪掉的 root（`Heap::begin_marking` 的 `watch_removals`、`drain_removed_roots`）：marking 開始時每個 root 都當場標上，之後加的 root 拿到的不是開始時的快照裡的（到得了它的路斷掉時由寫邊的 barrier 標上），就是 marking 期間配置的（黑的），刪掉它不會漏標；TLA+ 的模型拿掉它也成立。留著是因為 root 表大了之後要改成 concurrent 地掃 root（開始時不再全部標完），那時刪 root 的 barrier 是必要的。
- heap snapshot 的走訪在 young 的頁上跳過 filler（`heap/walk.rs` 的 `!= young::FILLER`）：拿掉時 filler 照樣不交出去，因為交出物件之前還看型別有沒有註冊（`types::is_registered`），filler 的 header 不是註冊過的型別；兩道擋同一件事，任一道都夠。
