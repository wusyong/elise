# `elise` 的 soundness

這份文件說明 `elise` 為什麼不會讓 safe code 造成 undefined behavior：要保證什麼、依賴哪些假設、用什麼守住，以及修改時要審查什麼。API 見 [`api.md`](api.md)，運作方式見 [`design.md`](design.md)。

## 結論

**目標**：不寫 `unsafe` 的程式碼只透過 `elise` 和 `elise-derive` 的 API，不可能造成 UB：use-after-free、讀到沒初始化的記憶體、型別混淆、違反 Rust 的 aliasing 規則、data race。

GC 的每一步都在安全點開始和收尾（scavenge、full GC 和 marking 的收尾都交給 heap 的 platform，可以有 worker），old 的 concurrent marking、concurrent sweep（含 `External` 的 finalizer）和空頁的 discard 在兩個安全點之間、和 mutator 同時在 worker 上跑；在下面的假設 G1–G3 下成立。scavenge 會搬動 nursery 的物件，論證的重點是搬動之後沒有指標指向舊的位置（I3、I10）；concurrent marking 的重點是 marker 和 mutator 同時碰的只有 atomic 的邊和 mark bit、marker 只讀寫好的物件（I4、I7），以及收尾時到得了的都標過（I3）；sweep 只讀交給它的頁的 mark bit（I7）；由 JS engine 推進的 marking 另外有 engine 的 thread 交物件的位址過來，論證在 I11。和 kun 的 `dom` 在 cppgc 上的論證不同，整個 heap 是 Rust 寫的，所以 Miri 能執行全部的程式：測試在 Miri 底下跑（Stacked Borrows、strict provenance），Miri 檢查的是那些測試走到的路徑沒有 UB；沒走到的路徑靠下面的不變式和人工審查。

## 假設

| | 假設 | 依據 |
| --- | --- | --- |
| G1 | `derive(GcObject)` 產生的程式碼正確：layout 列出每個 `Member` 欄位和它的 offset，`__init` 用每個欄位型別的 `HeapField::__write` 寫每個欄位 | 人工審查（見最後一節）、compile-fail 測試、mutation test |
| G2 | `HeapData` 的 impl 都正確：型別沒有指標、沒有 drop glue | 人工審查：只有 `crates/elise/src/data.rs` 裡的那些（數字、`bool`、`char`、`()`、陣列、`Option`、`Cell`、`RefCell`、`PhantomData`），`elise` 以外的 crate 不寫 `unsafe`，不能加 |
| G3 | 一個 process 裡同時活著的 heap 的 cage 不重疊 | 每個 cage 是自己的保留區（作業系統不會重複給同一段位址） |

## 不變式

**I1 哪裡有物件**：old 的頁上，GC 之外、也沒有 concurrent marking 在進行時，一格的 `marks` bit（large object 的 `marked`）設著，那一格就是一個完整的物件，header 是它註冊過的型別的編號：上一次 GC 留下的物件，和 debug build 之後配置的。`finalizable` bit 設著的，是有 drop glue、還沒 finalize 的完整物件。上一次 GC 之後配置的物件只經過邊和 root 讀（I2、I3），其他沒有 bit 的格子不讀：它的內容可能是死掉的物件、毒值或從沒寫過的位元組。

- 配置時物件先寫完，才寫 header 和設 bit（`Inner::publish`）：寫到一半 panic 的物件沒有 bit、不在任何邊上，下一次 sweep 不會掃它、不會 finalize 它，它的格子仍然是空區段的一部分（裡面已經寫進去的 `External` 會 leak，leak 不是 UB）。現在 safe 的 API 寫不出會 panic 的 init（init struct 的值在 `alloc` 之前就算好了，`Member` 的值只能是同一個 heap 的 `Gc`），這是多一層保險。
- 配置只從空區段拿格子，空區段是 sweep 從 `marks` 找出的 0（或新的頁的全部），所以不會把有物件的格子再給出去。新的物件的欄位都由 init 寫（`MemberArray` 自己把每條邊清成 0），不依賴格子原來的內容。
- Sweep 先清掉死物件的 `finalizable` bit、把頁整理好，才跑 finalizer：finalizer panic 時，剩下的死物件不會再被 finalize（leak），也不會 finalize 兩次。sweep 在背景跑時，有 heap 的 thread 那種（`LocalExternal`）finalizable 物件的頁和那種死掉的 large object 不交給 sweeper，由 heap 的 thread 在收尾之後的安全點照同樣的順序掃（`Inner::sweep_main_pages`）；只有 `Send` 的 finalizable 物件的頁和那種 large object 交給 job 的另一張清單，job 也是先清 bit、才跑 `Drop`；那之前這些頁沒有空區段，配置不會拿到死物件的格子。
- concurrent marking 進行中，cursor 和 scavenge 的 copier 拿的空區段整段標黑，還沒配置的格子也有 bit；收尾時（copier 是還回頁上時）清掉每段沒用完的，之後的 sweep 才讀 bitmap。進行中讀 bitmap 的只有 marker（設 bit）和 barrier，它們不把 bit 當成「這裡有物件」：只從邊和 root 找物件。
- 型別表的格子：static 型別的註冊之後不再改、永遠不釋放；embedder 執行時登記的型別（`raw`）的註冊之後只改退休的旗標，退休之後等每個當時活著的 heap 都做完一次之後才開始的 full GC 才釋放（I14）。
- nursery 的頁上，從頁的開頭到 `top`（目前那一頁的在 cursor 裡）一個接一個是物件和空段：物件是 header 加上資料、湊到 8 bytes；空段是 scavenge 的 thread 用不完的 LAB 的尾巴（header 的型別是 `FILLER`，flags 是它的 bytes）；搶輸的 copy 也留在頁上，是一份完整、沒有邊指向它的物件。有 drop glue 的列在 nursery 的 finalizer 清單上（`publish` 時，物件寫完之後）。nursery 的頁只有兩處從頭一個一個走：scavenge 掃 to-space 的 copy（都是完整的物件）和 heap verifier（測試用）。init 寫到一半 panic 的物件會在頁上留下沒有 header 的一段，verifier 會在那裡停下；現在 safe 的 API 寫不出會 panic 的 init（上面）。

**I2 邊**：物件的 `Member`、`WeakMember` 欄位，`EphemeronPair` 的 key 和 value，和 `HeapVector` 的陣列 `len` 以下的邊，是 0 或同一個 heap 裡一個活著的、型別相符的物件的 offset；陣列 `len` 以上的邊是 0。weak root 的表和 id 表的格子也一樣。weak 的邊和 weak root 不留住物件，所以它們的「活著」靠 GC 在釋放物件之前把它們清成 0（I3）；pair 的 value 只在 key 活著時被留住，key 是 0 時 value 也是 0。

- `Member` 只在 heap 裡出現：沒有建構子、沒有 `Clone`、`Default`，含有它的 struct 只能經過 `alloc` 建出來（init struct 裡是 `Option<Gc>`），物件只借出 `&T`，所以搬不出物件、也搬不進別的物件（compile-fail 測試）。
- 邊只從 `Gc<'gc, T>` 寫入（`compress`）。`Gc<'gc, T>` 只從同一個 heap 的 `alloc`、`Member::get`、`WeakMember::get`、`EphemeronPair` 的 `key`、`value`、`Persistent::get`、`WeakPersistent::get`、`Mutation::by_id`、`HeapVector::get` 來；`by_id` 的型別由物件 header 的型別編號檢查（`Erased` 不檢查，它沒有欄位可讀），id 表的格子不記型別；`'gc` 是 invariant 的，每個操作各有一個，所以兩個 heap 的 `Gc` 混不在一起。執行期另外檢查：`compress` 檢查 `Gc` 在這個 cage 裡，`Member::get`、`set` 檢查 `Member` 在 `cx` 的 cage 裡，`Persistent::get` 檢查是同一個 heap 的 root 表；這些檢查一直開著。
- `HeapVector` 的元素型別 `T` 不在 layout 裡（每個 `T` 共用一個 `TypeSlot`），只由 `HeapVector<T>` 的方法的型別保證；陣列新配置時每條邊清成 0，`remove` 把最後一條清成 0。
- `Member<Erased>` 的邊指向任何型別的物件，「型別相符」對它不限型別：`Erased` 不是 `GcObject`、配置不出來，`Gc<'gc, Erased>` 只從 `Gc::erase`（同一個物件、同一個操作）和 `Member<Erased>`、`Persistent<Erased>` 的 `get` 來；它借出的 `&Erased` 是零大小、沒有 bytes 可讀；轉回別的型別只有 `Gc::<Erased>::downcast::<T>`，它比對物件 header 裡的型別編號，是 `T` 的才給 `Gc<'gc, T>`。Mark 照物件自己 header 裡的型別走，不照邊的型別，所以經過 `Erased` 的邊照樣標得到、走得對。
- `[Member<T>; N]` 的欄位是 `N` 條連續的邊：`Member` 是 `repr(transparent)` 的 `AtomicU32`（std 的，loom 和 shuttle 底下也是），陣列的元素一個接一個、每個 4 bytes；layout 記成 `FieldKind::Members(N)`，init 的每一格經過 `compress` 寫進去。`WeakMember` 和 `[WeakMember<T>; N]` 同樣，記成 `Weak`、`Weaks(N)`；`EphemeronPair` 是 `repr(C)` 的兩個 `AtomicU32`（key、value），記成 `Ephemeron`、`Ephemerons(N)`（8 bytes 一個）。`FieldKind::edges` 不數它們，所以 marker、scavenger 和 verifier 照 strong 的邊走時不會經過 weak 的。

**I3 活著的物件**：GC 之後，從 root 表走得到的物件都還在；`Gc` 不會活過它的操作。

- 一個操作裡不會 GC（GC 只在 `Heap::mutate` 開始時、`Heap::collect`、`Heap::scavenge` 和 engine 推進的 marking 的步驟，都要 `&mut Heap`，而 `Mutation` 借著 `&Heap`）。`Heap::read` 的操作也只借 `&Heap`，所以可以一個在另一個的 `f` 裡；兩者都不 GC，裡面的配置經過 `Inner` 的 interior mutability，和沒有重疊時一樣，各自的 `'gc` 讓一個操作的 `Gc` 不能交給另一個的 `Mutation`。
- `Gc<'gc, T>` 的 `'gc` 是 `mutate` 的 closure 的 higher-ranked lifetime，回傳值和外面的變數都裝不下它（compile-fail 測試）。所以 GC 時 heap 外沒有任何指向物件的指標，除了 root 表。
- Mark 從 root 表的每一格出發，照 I2 走每條邊。
- Concurrent marking（SATB）：開始時 nursery 是空的（先做一次全部升上去的 scavenge），每個 root 當場標上。之後 old 的物件被留著，是因為下面之一：開始時從 root 走得到、而那條路一直沒斷（marker 走到它）；那條路上的一條邊或 root 被改掉（barrier 標上舊值、推給 marker）；marking 期間配置或升上來的（黑的）。marker 跳過 young 的物件：young 物件指到的 old 物件，不是快照裡的就是黑的。收尾時清空 barrier 的段、做完 worklist，所以到得了的 old 物件都標過。這個論證在 TLA+ 的模型（young 和 old、marking 期間的 scavenge、跳過 young）上對兩種大小窮舉過，拿掉 barrier、black allocation、升上來的算黑、開始前的 scavenge 任何一個都找得到反例；heap verifier 每次收尾檢查。
- **Weak 的邊和 root 在物件釋放之前清掉**：
  - full GC：收尾時，marker 記下的 weak 格子（它掃的物件的 weak 欄位裡，目標是 old、掃的時候還沒標的）中目標還沒標的清成 0，weak root 的表整張看一遍，目標是 old、沒標的清成 0；之後才 sweep。沒記下的 weak 格子不會指向死物件：marker 掃的物件裡，掃的時候目標已經標過的一直標著；之後寫進去的值，SATB 下收尾時都標過（mutator 拿得到的物件只有快照裡的、marking 期間配置或升上來的、weak 讀出的，三種都會被標上）；marking 期間配置和升上來的物件（黑的，marker 不掃）的 weak 欄位都是 marking 期間寫的（開始前 nursery 清空了），同樣標過。marker 沒掃的、開始前就在的物件，就是沒標的，會被 sweep。這段論證在 TLA+ 的模型上窮舉（下面「怎麼驗證的」），verifier 每次 full GC 之後檢查活物件的 weak 欄位和 weak root。
  - weak 讀取的 barrier：marking 期間 `WeakMember::get`、`WeakPersistent::get` 讀到 old、還沒標的物件時，用 SATB 的同一段標上、推給 marker（它可能已經不在快照到得了的範圍，但現在是 mutator 的）。少了它，讀出來的物件寫進掃過的物件之後沒有被標，收尾時被收掉（TLA+ 的反例）。marking 以外的時間不需要：收尾在停頓裡清掉了 weak，讀到的都是活的。
  - scavenge：weak 的邊不當 root。copy 完之後，copy 的 weak 欄位（掃 copy 時記下）、weak 的 slot set 記著的 old 物件的 weak 邊（I10）和 weak root：指向 from-space 的，有去向的改成 copy，沒有的（死了）清成 0；之後 from-space 的頁才放掉。
- **Ephemeron 的 value 只在 key 活著時留下，key 死了時兩個一起清掉**：
  - full GC：marker 掃 pair 時 key 標過（或是 young）就標 value，不然把 pair 記成 discovered；沒有東西可標時再看 discovered，key 標過的標 value，反覆到沒有新的（背景的 run 和收尾都是），所以收尾時 key 標過的 pair 的 value 都標過。discovered 剩下的 key 沒標，收尾時兩個一起清成 0，之後才 sweep。沒記成 discovered 的 pair：掃的時候 key 已經標過、value 也標了；之後寫進去的 key 和 value 收尾時都標過（同 weak 的論證）；黑的物件的 pair 都是 marking 期間寫的。value 的寫入經過 SATB（被覆寫的 value 可能是快照到它的唯一一條路）；value 的讀取先經過 key 的讀取 barrier：讀出的 value 會被 mutator 拿去用，key 標上之後 value 跟著標上。
  - scavenge：value 當 strong 的邊（所以搬動之後它指的是 copy），key 照 weak 處理：死了就連 value 一起清成 0。之後 from-space 的頁才放掉。
- **id 表的格子在物件釋放之前清掉**：full GC 收尾時整張表走一遍、指向 old 而沒標的清成 0，之後才 sweep；scavenge 走 young 的清單（`new_id` 時物件是 young 的就列進去，old 的物件不會變回 young），指向 from-space 的改成 copy 或清成 0，之後 from-space 才放掉。`by_id` 在 marking 期間經過 weak 讀取的 barrier。所以表的格子是 0 或活的物件，`by_id` 讀 header 是讀活的物件。`raw::EngineHeap::new_id` 是 `unsafe` 的：給的 bits 要是這個 heap 的活物件，表的格子才是活的物件（young 的列進清單也靠它讀對 bits 落在哪裡）；`id_lives` 只看格子是不是 0、不讀物件、不給物件，所以不需要讀取的 barrier（marking 還沒標到的物件回 true，不會因此被留下）。
- Scavenge 搬動 nursery 的物件：從 root 表的每一格、slot set 記著的每條邊（I10）出發，copy 到 to-space 或 old，舊的位置寫上搬到哪裡（header 的第一個 word，flags 是 `FORWARDED`），所以每個物件只 copy 一次。copy 的 header 是同一個型別，flags 只留 site 的編號（高 16 bit）和留在 nursery 的 `AGED`；讀 flags 的地方只看 `FORWARDED`、`AGED` 和填空段的長度，site 的編號只拿來計數（verifier 檢查 old 的物件沒有別的 flags、site 是這個 heap 配置過的）。root 表的每一格、slot set 的邊、每個 copy 的邊都改成新的位置；GC 時 heap 外沒有別的指標（上一條），所以搬動之後沒有指標指向 from-space：old 物件指向 young 的邊都在 slot set 上（I10），young 物件的邊只有 copy 的那些（死的 young 物件不再被讀）。full GC 先做一次把活的全部升到 old 的 scavenge，mark-sweep 只看到 old 的頁。

**I4 Aliasing**：物件只以 `&T` 借出，沒有 `&mut T`。會變的欄位都是 `Cell` / `RefCell`（`HeapData` 的 impl 只有這兩種有內部可變性）、`Member` 和 `WeakMember` 是 `AtomicU32`，`HeapVector` 的陣列只經過從 cage 的指標算出的 `&AtomicU32`（`from_ptr`）讀寫，從不建立涵蓋整個陣列的 `&MemberArray`。GC 只在沒有任何借用時（`&mut Heap`）寫 header 和讀 bitmap 找物件；mark 只讀 header 和 `Member`、`WeakMember` 欄位（`AtomicU32` 的位元組），不讀其他欄位：其他欄位可能有 padding（沒初始化的位元組），concurrent marking 時 mutator 也可能正在改它們。concurrent marking 的 marker 和 mutator 同時碰的只有邊（兩邊都是 atomic 的讀寫）和 mark bit（atomic）；header 和 `MemberArray` 的 capacity 在物件發布之前寫好、之後不改（scavenge 改 young 的 header 時 marker 停著），marker 只讀它從邊、root 或 barrier 拿到、自己標上的物件（I7），所以普通的讀不是 data race。

**I5 Provenance**：所有指向物件的指標都從 cage 的起點（`Cage::base`，保留區的指標）加上 offset 算出（`wrapping_add`），不從整數造指標；header 存的是型別的編號，不是指標。`-Zmiri-strict-provenance` 底下跑測試守住。

**I6 Finalizer 碰不到 heap**：`External<T>` 和 `LocalExternal<T>` 要 `T: 'static`，`Gc` 裝不進去；物件自己不能實作 `Drop`（derive 產生的兩個 impl 會衝突，compile-fail 測試），`HeapData` 沒有 drop glue（G2），所以物件唯一的 drop glue 是這兩種的值的。**在別的 thread 上跑的 finalizer**：只有 drop glue 全是 `External` 的值的型別（`HeapField::MAIN_THREAD_DROP` 都是 false：`HeapField` 是 `unsafe trait`，它的契約要求 drop glue 不能在別的 thread 跑的欄位標成 true；`LocalExternal` 標 true，`External` 要 `T: Send`，其他的 impl 沒有 drop glue）在 sweep 的 worker 上 finalize，所以那裡跑的 `Drop` 只碰 `Send` 的值；死物件沒有人讀（不可到達，它的頁在結果套用前 heap 的 thread 不碰），bit 先清掉再跑，只跑一次；heap 的 thread 不在操作中途跑它們（配置時自己掃的頁不含要 finalize 的）。finalizable 的 bit 在 `Marks` 裡、是 atomic 的 word：交給 worker 的頁只有它改，heap 的 thread 在套用之後才讀寫（verifier 不在那之前數它們）。Finalizer 可以拿著 `Persistent`（`'static`）並 drop 它：那只是改 root 表，finalizer 跑的時候沒有人拿著表的內容（下面的 I13）。Finalizer 拿不到 `&mut Heap`（要的話只能經過 `Rc<RefCell<Heap>>`，那會在 `borrow_mut` 時 panic）。

**I7 Thread**：`Heap`、`Gc`、`Persistent` 都不是 `Send` / `Sync`（`Rc`、raw pointer：`Persistent` 拿著 root 表的 `NonNull`，表在 `UnsafeCell` 裡）。跨 thread 的有四樣：
- **型別表**：註冊在一個 `Mutex` 底下，`TypeSlot` 的編號和表的格子用 release 寫、acquire 讀。執行時的型別的登記、退休、釋放，編號的保留和歸還，和每個 heap 的 full GC 的計數，都在另一個 `Mutex`（`types.rs` 的 `Runtime`）底下；釋放的 `Entry` 在放掉鎖之後才 drop（embedder 的 keepalive 可能再呼叫進來），keepalive 要 `Send + Sync`：在哪個 thread 釋放不一定。
- **Marking**：GC 在 heap 的 thread 上、`Heap::collect` 或 `mutate` 裡（`&mut Heap`，沒有 `Mutation`），把 `Marking` 交給 platform；platform 的 `run` 等所有的 worker 都返回才返回（`Job` 的契約，`ThreadPool` 照做）。期間 worker 只經過 `Base`（cage 的起點）讀物件的 header 和邊、經過頁的狀態設 mark bit；沒有任何 thread 寫 header 和邊（heap 的 thread 在 `run` 裡），所以普通的讀不是 data race。mark bit 是 atomic 的 or（relaxed）：只要求同一個 bit 只有一個 thread 看到「原本沒標」；worker 開始和結束時經過 platform 的 mutex，所以 heap 的 thread 在 `run` 之後讀得到所有的 mark bit。worklist 的段經過它的 mutex 交接。
- **Concurrent marking**：marker（`crates/elise/src/heap/concurrent.rs` 的 `Marker`）是 `post_job` 交出去的 `Arc<dyn Job + Send>`，帶著 `Base`、mark bit 的表（`MarkTable`）、young 的表和 worklist，不借 `Inner`。它在 mutator 跑的同時：
  - **讀邊**：relaxed 地讀，背景的 run 每讀一批（16 條）做一次 acquire 的 fence。mutator 在 marking 期間先過 barrier、再 release 寫邊；marking 期間新配置的物件在被寫進任何邊之前已經寫完、mark bit 已經設上（black allocation：整段在 cursor 拿到時就設上，large object 配置時設），所以 marker 看到指向它的邊時，經過 release 和 fence 也看得到它標過，不會去掃它（loom 的模型窮舉這個順序）。marker 只掃 `mark` 回傳「原本沒標」的物件：marking 開始前就在的（開始時經過 `post_job` 的 mutex 交接，寫好的）。
  - **mark bit**：每頁的 `Marks` 在頁自己的配置裡（`OwnedMarks`），不搬；不用 `Box`：`Box` 每次跟著頁搬動都宣稱它是唯一的指標（Stacked Borrows 的 unique retag），`MarkTable` 的指標就失效了（下面「找到並修正過的問題」）；`MarkTable` 是 heap 上一張固定大小的 atomic 指標的表，頁在用之前（在 mutator 的 thread 上）release 寫進去，只在安全點拿掉（job 已經取消、沒有 marker 在跑；交給 sweep 的頁在它的結果套用之後）、之後才釋放頁。表用 relaxed 讀：marker 碰到的頁不是開始前就在的（交接的 mutex），就是經過上一條的 release 和 fence 看到的邊所在的。bit 都是 atomic 的 or 和 and。
  - **停下**：scavenge、收尾和 heap 的 drop 前先 `cancel` job，`cancel` 等它的 run 都返回（`ThreadPool` 的 `PostedJob`：設旗子，等 `active` 歸零，再拿掉），之後才搬動物件、sweep 或釋放 cage；所以 job 碰 cage 的時候，cage 和它碰的頁都還在。`PostedJob` 在 unwinding 時被 drop 只設旗子、不等（等的話一個 run 的 panic 會變成 abort）：heap 的 drop 先明確地 `cancel`，所以 job 不會比 cage 活得久。
  - **`Base` 的 `Send`、`Sync`**：只用來讀 header 和邊（上面的條件下不是 data race）；scavenge 的 thread 經過它讀寫時，沒有別的東西在跑。
  - **concurrent sweep**（`crates/elise/src/heap/sweep.rs` 的 `Sweeper`）：收尾時把 size class 的頁的編號交給 `post_job` 的 job，job 經過 `MarkTable` 只讀那些頁的 mark bit、算出空區段和活的數目，放進 mutex 底下的清單；頁的狀態（`Space`）只有 heap 的 thread 在安全點和配置的慢路徑套用結果時改。交給 sweep 的頁在套用之前沒有空區段、不在 `available`，所以 mutator、scavenge 的 thread 不在上面配置，它的 mark bit 也沒有人寫（publish 只設在用的頁，black allocation 只在 marking 期間，marking 開始前 `finish_sweeping` 先取消 job、用 `run` 掃完、全部套用）；heap 的 thread 自己掃一頁時先在同一個鎖底下把它從清單拿走。頁只在它的結果套用之後才可能被釋放（整頁空時進 pool，`MarkTable` 的格子那時拿掉）。heap 的 drop 先 `cancel` 它。
  - **ephemeron**：marker 讀 pair 的 key 和 value 和讀邊一樣（一批之後 fence），discovered 的清單經過 mutex 合起來、拿出來看（看的時候同樣讀完一批再 fence）；收尾時有新標的 value 就用 `Worklist::reseed` 再跑一次 `run`。
  - **weak 的格子**：marker 讀 weak 欄位和讀 strong 的邊一樣（relaxed、一批之後 fence；mutator 在 marking 期間 release 寫 weak 欄位），只看目標的 mark bit、不往下走；記下的格子在每個 run 的清單裡，run 結束時經過 mutex 合起來，收尾時在停頓裡（job 取消之後）由 heap 的 thread 清。
  - barrier 推的物件在 mutator 本地的段，滿了或在安全點經過 worklist 的 mutex 發布；`done` 的判斷在 worklist 的鎖底下讀「沒有段」和背景的 run 的數目（run 拿段之前先加），所以拿著最後一段的 run 一定被看到（loom 的模型）。
- **Scavenge**：和 marking 一樣經過 platform 的 `run`，heap 的 thread 在 `run` 裡（`&mut Heap`，沒有 `Mutation`），`run` 返回時所有的 thread 都做完了。期間：
  - from-space 物件的 header 只用一個 64 位元的 atomic 讀和 CAS（`forward`，acquire 讀、acq-rel 換）；物件的資料在 scavenge 期間不變，複製它不是 data race。每個 thread 先複製、再 CAS 寫上去向，只有一個 CAS 成功，輸的 thread 用贏家的去向，自己那份只在它是自己 LAB 或區段的最後一份時退回，不然留著（完整、沒人指著）。heap 的 thread 在 `run` 之後才用普通的讀看 header。
  - 每個 thread 只寫自己的 LAB 和自己的每一級的區段（從 heap 的鎖底下拿：`Source` 在 mutex 裡，頁的狀態、pool、commit 都在裡面），寫完一份 copy 才把它推進 worklist，別的 thread 經過 worklist 的 mutex 拿到它，所以看得到它的內容。每條邊只屬於一份工作（一份 copy 的邊、一頁 slot set 的格子、root 表），只有拿到那份工作的 thread 讀寫它。
  - slot set 在 heap 上一張每頁一格的表裡（不在頁的狀態裡，那個陣列可能在別的 thread 拿新頁時變長），bitmap 用 CAS 換上去、bit 用 atomic 的 or 設；young 的表的格子是 std 的 `AtomicBool`，拿 to-space 的頁時在鎖底下設上，贏家的 CAS 是 release，看到去向的 thread 也看得到那頁是 young。
  - `Cage` 標成 `Sync`：它的指標在 `new` 之後不變，`commit` 和 `discard` 只改作業系統的映射，作業系統替它們排序；commit 一次只有一個 thread 在鎖底下呼叫，worker 只 discard 沒有 thread 會 commit 的頁（I9）。
  - root 表的格子在 `run` 之前拿出來放進 mutex，只有呼叫的 thread（第一份工作是 root）更新，`run` 之後放回去。
- **`ThreadPool` 的工作**：`run` 把 `&dyn Job` 的 lifetime 換成 `'static` 放進 pool 的狀態，這是 `ThreadPool` 唯一的 `unsafe`。它在 `run` 返回前、沒有 worker 正在跑它（`active` 是 0）而且不會再有 worker 開始（`closed`）時清掉，所以沒有參照活得比 `run` 久。呼叫的 thread 自己的那次 panic 也先被接住，等 worker 都返回才繼續往上。`post_job` 的工作是 `Arc`，本身就是 `'static`，不用換 lifetime；pool drop 時先取消還在的，worker 才會返回。

`raw::EngineHeap` 是 `Send` 和 `Sync`，裡面的 `Heap` 不是：`Heap` 不是 `Send` 是因為操作留在 thread 上的東西（`Persistent` 用位址拿著 root 表，heap 換了 thread 之後在原本的 thread 上 drop 它，會和 GC 同時改表；`LocalExternal` 的 finalizer 在 heap 的 thread 上 drop 不是 `Send` 的值）。`EngineHeap` 不跑操作、不借出 `Heap`，所以它擁有的東西只有經過它才拿得到，跟著它一起換 thread；platform 的 worker 拿到的工作本來就是 `Send`。`Sync`：`&self` 的方法只讀 cage 的開頭、frame 的計數、用量的計數（`Cell` 的 `get`，`Cell` 只在拿著 `&mut self` 的呼叫裡、由 heap 自己的 thread 寫）和物件的 header、長度、邊（邊用 atomic 讀，和 worker 一樣），不借任何 `RefCell`；其他的方法都要 `&mut self`。給 `EngineHeap` 加 `&self` 的方法時，要確認它不寫 `Cell`、不借 `RefCell`。

`EngineHeap::barrier_state` 給 engine 的碼一個指標，碼不經過 Rust 讀它指到的 barrier state 和 young 頁的旗標，所以讀的時候不能有別的 thread 在寫：

- barrier state 是 `Inner` 擁有的 `Box`，位址在 heap 活著時不變，`EngineHeap` 換 thread 也不變；young 頁的旗標是 `Inner` 拿著的 `Arc` 的陣列，barrier state 的指標指著它，和它活得一樣久。`barrier_state()` 只取位址、不讀內容，所以是 `&self` 的。
- marking 的旗標（`Cell<bool>`）只在 `marking_active` 改的地方跟著改，都在 heap 自己的 thread 上、拿著 `&mut self` 的呼叫裡；碼在拿著 `&mut self`（wasmtime 的 `Store`）的 thread 上跑，所以讀和寫在同一個 thread、不同時。
- young 頁的旗標只在 scavenge 和 nursery 拿頁、還頁時寫：都在 heap 的 thread 上的 GC 或配置的呼叫裡，平行 scavenge 的 worker 寫的在呼叫返回前 join。碼讀的時候沒有那些呼叫在跑；concurrent marking 的 worker 同時讀，讀和讀不衝突。新增在 heap 的呼叫之外（例如 worker 在 concurrent marking 期間）寫旗標的路，碼的讀就成了 data race，要改成 atomic 的讀或換別的做法。
- `satb`、`remember` 的前提就是 `write_bits_in` 的：值是 0、`I31` 的或活物件的 bits，格子是活物件的邊。碼要照協定做：漏掉一次 `satb` 或 `remember`，就可能留下指向已經放掉的物件的邊，和不經過 barrier 寫邊一樣，這是 engine 那一側的前提，elise 驗證不了（wasmtime 的 fork 用測試和審查守著）。多呼叫不會出錯：`satb` 自己看 marking 的旗標，`remember` 自己再檢查值和格子。

**I8 大小和對齊**：`TypeInfo::of` 在編譯期檢查物件最多 8 bytes 對齊、最大是 cage 的四分之一。格子 16 bytes 對齊、large object 從頁的開頭開始、nursery 的頁從頭以 8 bytes 為單位 bump，header 8 bytes，所以物件 8 bytes 對齊。nursery 只放一格放得下的物件（`MAX_SLOT_BYTES` 以下），升到 old 時一定有它的 size class；更大的一開始就是 large object。邊的 offset 小於 4 GiB，指到的頁是在用的 size class 的頁或 large object 的第一頁（I2、I3：活的物件不會在被釋放的頁上），從 offset 算第幾格的乘法對每個頁內的位移都精確（單元測試窮舉），debug build 另外檢查算出的是格子的開頭。

**I9 放回作業系統的記憶體**：discard 只在頁沒有物件時做，而且要等它進 pool 之後再過一輪 full GC：一個 GC 釋放的頁上，死物件的 finalizer 和 debug 的毒值都在同一個 GC 裡跑完，那時記憶體還在。scavenge 之後 from-space 的頁也進 pool（標成下一次 full GC 的編號，最早在那之後的一次 discard），nursery 的死物件的 finalizer 在 from-space 的頁上、毒值之前跑。被拿去用的頁先 commit。platform 有 worker 時，discard 由 worker 做（`crates/elise/src/heap/discard.rs`）：sweep 把要 discard 的頁從 pool 拿出來、標成 `Discarding` 交給它，配置、large object 和 scavenge 只拿 `Free` 的頁，所以 worker 還在 discard 的頁沒有 thread 會 commit 或寫；worker discard 完放進「做完」的清單，之後的安全點（`mutate`、`collect`、`scavenge` 開始時）才把它改回 `Free`、列進 discard 過的頁。worker 經過 `Discarder` 拿到 cage 的位址（`Send`、`Sync`，只有一個不變的指標），heap drop 時先取消它的 job（`cancel` 等它的 run 都返回），cage 才放掉。

**I10 Remembered set**：old 物件（活的或還沒 sweep 的死物件）的每條指向 young 物件的邊，都在它那頁（large object 是它自己）的 slot set 上；weak 的邊在另一張 weak 的 slot set 上，pair 的 key 在第三張 ephemeron 的 slot set 上（scavenge 清 key 時要一起清 value，所以和 weak 的分開），pair 的 value 和 strong 的邊一樣在 slot set 上。三種 slot set 記著的格子都在沒被 sweep 釋放的物件裡。

- 寫邊都經過 write barrier（`Inner::write_barrier`：寫進去的值在 young 的頁上、被寫的格子不在 young 的頁上時，記進 slot set）：`Member::set`、init 時的 `Member` 和 `[Member<T>; N]`（物件可能因為 nursery 滿了、太大或在 `pretenured` 裡而配置在 old）、`HeapVector` 的 push、set、remove 和長大時的複製。weak 的邊同樣經過 `Inner::weak_barrier`（`WeakMember::set`、init 時的 `WeakMember` 和 `[WeakMember<T>; N]`），記進 weak 的 slot set；pair 的 key 經過 `Inner::ephemeron_barrier`，value 經過 `write_barrier`；scavenge 升上去的 copy 的 weak 邊還指向 young 的，scavenge 最後記進去。`MemberArray` 新配置時邊清成 0，不指向任何物件。判斷 young 讀的是一張每頁一格的表，只在安全點改。
- Scavenge 把每頁的 slot set 拿出來、更新每條邊，還指向 young（to-space）的才放回去；升到 old 的 copy 掃邊時，指向 young 的也記進它那頁的 slot set。平行時一頁的 slot set 可能在升上去的 copy 記了新的一格之後才被處理，那格已經指向 to-space：所以「指向 young」看的是 young 的表（to-space 的頁是 young），不是「是不是在 from-space」（fuzz 找到的，`crates/elise/fuzz/regressions/young-slot-order.bin`）。- **sweep 不看 slot set，所以釋放的格子上不能有記錄**（sweep 在背景跑時也是）：full GC（stop-the-world 的和 concurrent marking 的開始）先做一次把活的全部升上去的 scavenge，nursery 變空，兩種 slot set 都沒有放回去的。之後到 sweep 之間記下的格子：write barrier 記的在 mutator 拿得到的物件裡（快照裡的、黑的），scavenger 記的在升上來的 copy（黑的）裡，收尾時都標過，不會被 sweep。所以 sweep 釋放的格子上沒有記錄；verifier 每次 full GC 之後檢查。原本靠「full GC 先清空 nursery」，concurrent marking 收尾時 nursery 不是空的，換成這個論證。
- 死的 old 物件的邊在下一次 full GC 前照樣被 scavenge 當成 root 更新：多留住一些 young 物件（之後 full GC 收掉），但它指的一定是有效的 copy。V8 在 marking 之後過濾 remembered set；這裡不用：上一條的論證下，收尾時有記錄的物件都標過，sweep 在背景跑、scavenge 同時讀 slot set 時，還沒掃的頁上的死物件沒有記錄（TLA+ 的 `SlotsLive` 窮舉，下面「怎麼驗證的」）。

**I11 和 JS engine 一起標記**（`crates/elise/src/js.rs`）：engine 的 wrapper 記著的位址，在 wrapper 活著時一直是這個 heap 的一個活的、沒搬過的物件；engine 推進的 marking 收尾時，活的 wrapper 的物件都標過。

- **不搬**：driven 的 heap 裡帶 `JsEdge` 的型別一律配置在 old（`alloc_at`），old 不 compact，所以位址不變。`engine_address` 只給 driven 的 heap、帶 `JsEdge` 的型別。
- **活著**：wrapper 活著時，engine 的 marking 標到它就交出物件（`WrapperLocal::hand` 當場標上），所以收尾時物件標過、不被 sweep；marking 期間新 wrap 的物件是 mutator 在操作裡拿著的，SATB 下收尾時一定標過（快照裡的、黑的配置或讀取的 barrier 標的）。wrapper 和物件都沒人到得了時，同一次 major GC 兩邊一起收，engine 先放掉沒標到的 `TracedReference`。這段協定（加上 engine 那一側的 Dijkstra、engine 的 scavenge 丟掉沒改過的 wrapper 和互相交的邊）在 TLA+ 的模型上窮舉過，拿掉交接、JS 的邊的交接、讀取的 barrier、讓年輕的物件被 wrap、兩邊都做完才收尾、engine 丟掉時 reset 那一格任何一個都有反例。
- **engine 錯了**：交過來的位址不在這個 heap 的 cage 裡的，`hand` 不理、`from_engine` panic；在 cage 裡但不是活的物件的，是 `hand` 和 `from_engine` 的 `unsafe` 的契約（呼叫的一方保證）：接上 engine 的那一側要守住它（例如一種 tag 一種型別、只有它用那個 tag wrap），寫進它自己的 soundness 的論證。
- **engine 的 thread**：`WrapperMarker` 帶著 marker（`Arc`，它的 worklist 和 mark bit）。engine 的 thread 只做兩件事：`fetch_or` 那個物件的 mark bit、把它推進 worklist（它的 mutex）。不讀物件本身；物件是 old 的，scavenge 不動它。mark bit 在 heap 的頁上，所以 marking 收尾（`end_marking`）和 heap 丟掉時先 `retire`：關上 marker、等進行中的交接做完，之後交過來的什麼都不碰（engine 的 thread 可能比 marking 和 heap 活得久：isolate 在標記途中拆掉）。交接先把計數加一、再看開著沒，`retire` 先關、再等計數歸零，兩邊都是 SeqCst，所以兩者之一一定看得到對方：不會有交接在 `retire` 回來之後還碰到頁。
- **JS 的邊**：`JsEdge` 的 8 bytes 是 engine 的，heap 建成 0 之後不讀不寫（沒有 engine 驅動的 heap 裡它們是 embedder 的 word，經過 `embedder_word`、`set_embedder_word` 在操作裡讀寫：沒有 engine 拿著它的位址，marker 也不交出、不讀它，所以只有 heap 的 thread 碰它；底下是 `AtomicU64`，driven 的 heap 裡這兩個 panic，不會蓋掉 engine 的參照）；marker 掃到物件時把它的位址交給 `JsEdgeSink`（`JsEdgeLocal::mark` 是 `unsafe`：heap 保證那是它標過的物件的邊，物件活到 marking 結束、不搬）。sink 的 local 只在 marker 的 run 和 heap 的 thread 的步驟（`Marker::step`）裡建、結束時 drop，所以 `advance_marking` 回報做完、`finish_marking` 之後沒有 local 活著：engine 那一側可以靠這一點，只在它標記期間建自己的 marker（`JsEdgeSink` 的文件寫著）。物件死了不用碰那一格：engine 收尾前已經放掉沒標到的參照。
- **driven 的 heap 不自己做 full GC**：heap 不知道 engine 的哪些 wrapper 還活著，`collect` panic；driven 的 heap 也從不自己開始 old 的 marking。

**I12 token 的 cell**（`crates/elise/src/token.rs`）：一個 `TokenCell` 的值同時只有一個 `&mut`，或任意個 `&`，不會兩者都有。

- 一個 thread 同時只有一個 `LocalToken`：`new` 把 thread-local 的旗標從 false 換成 true，已經是 true 就 panic；`Drop` 換回 false。token 被 drop 時沒有它的借用活著（借用活不過它），所以之後再建一個不會和之前的重疊；`mem::forget` 掉的 token 只讓這個 thread 不能再建，不是 UB。旗標是 `const` 初始化、沒有 drop 的 thread-local，thread 結束時也讀得到。
- `LocalToken` 不是 `Send`、不是 `Sync`（`PhantomData<*const ()>`），所以 `&` 和 `&mut` 的 token 都只在它的 thread 上。`TokenCell` 不是 `Sync`（`UnsafeCell`），所以 `&TokenCell` 不離開它的 thread；它是 `Send`（值是 `Send` 時），但搬走要擁有它，那時沒有借用。所以一個 cell 的每次 `get`、`get_mut` 都在同一個 thread 上、用同一個 token。
- `get` 借 token 的 `&`、`get_mut` 借 token 的 `&mut`，回傳的參照活不過那個借用：borrow checker 保證一個 `&mut` 的期間沒有同一個 token 的其他借用，也就沒有任何 cell 的其他參照（compile-fail 測試）。
- 不靠 heap、`'gc` 或 cage：cell 可以在 heap 外（stack 上、leak 成 `&'static`），論證照樣成立。曾經考慮過 token 綁在操作的 `'gc` 上、每個操作一個：`'gc` 不把 cell 綁到 heap，heap 外的 cell 可以在 Dom A 的讀裡用 A 的 token 借著、再用 Dom B 的 `&mut` token 換掉，所以沒有採用。
- 在物件裡時它在 `External` 裡（值是 `Send`），sweep 的 worker 跑它的 `Drop`：那時物件已經沒有人到得了，沒有操作拿著它的參照（I1、I3）。`replace` 把舊的值交還給呼叫的一方，不在借著 token 時 drop。


**I13 root 表**：`Persistent`、`WeakPersistent` 只記 heap 的 root 表的位址和一格的編號（Blink 的 `PersistentRegion` 的節點），不計數。
- 表在 heap 自己的一塊配置裡（`OwnedShared`：`Box::leak` 出來的 `NonNull` 加上自己的 `Drop`，和 `OwnedMarks` 一樣），heap 搬動時它不跟著搬，位址在 heap 活著時一直有效；不用 `Box` 是因為 `Box` 的每次搬動在 Stacked Borrows 下是一次 unique retag，會讓 root 拿著的指標失效（heap 跟著拿著它的 struct 搬，`crates/elise/tests/it/heap.rs` 的 `a_heap_that_moves_keeps_its_roots` 在 Miri 下守著）。
- 契約是 root 不活得比 heap 久。違反時不是 UB：`Heap` 的 drop 在最後（finalizer 跑完、它們拿著的 root 也放了之後）數兩張表還在用的格子，有的話把 `Shared` 留下不放（`OwnedShared::leak`），之後那些 root 的讀和 drop 寫進的還是有效的記憶體；debug build 接著 panic（unwind 中不 panic），找出放的順序錯的地方。先留下再 panic，panic 的 unwind 途中 drop 的 root 才不會碰到放掉的表。
- 表的內容（`Entries`）在 `UnsafeCell` 裡，`RootTable` 因此不是 `Sync`，只有 heap 的 thread 碰它（`Persistent` 不是 `Send`）。每次存取經過 `with`：它的 closure 是 `RootTable` 方法自己的本體，裡面只動 `Vec`，不呼叫別人的程式（沒有 callback、不 drop 會碰表的東西），也不把參照帶出 closure，所以同一時間只有一個 `&mut Entries`，沒有重疊的借用。原本的 `in_use` 回傳借著表的迭代器、`entries_mut` 回傳 `&mut [u32]`，現在一個收集成 `Vec`、一個改成拿出來改完放回去，GC 的程式在走它們的時候 drop 了 root（今天沒有這種地方）也只是改到別的東西、或在空的表上越界 panic，不是 aliasing。
- scavenge 照舊把格子拿出表（`take_entries`）放進 job 的 mutex，跑完放回去（I7）；那段時間 mutator 不跑，沒有 root 被建立或放掉。

**I14 執行時的型別**（`crates/elise/src/raw.rs`、`crates/elise/src/types.rs` 的 `Runtime`）：embedder 執行時登記的型別的物件，GC 讀的只有 header、layout 列的邊和 array 的長度，都在物件裡；型別的 `Entry` 在 GC 還可能讀到它的物件的 header 時都還在。`raw` 有 `unsafe fn`：和 `from_engine`、`WrapperLocal::hand` 一樣，是給 embedder（wasm engine）的，它的契約由 embedder 守；safe 的部分（登記、配置、`data`、`index_of`）照樣不會造成 UB。
- 登記時檢查 layout（`raw::check`）：大小加上 header 不超過物件的上限；邊 4-aligned、在大小以內、遞增（不重疊）；array 的長度是固定部分裡 4-aligned 的 `u32`、不是邊；元素是 1、2、4、8 或 16 bytes，固定部分對齊到它（最多 8）；元素是邊時是 4 bytes。物件的資料 8-aligned，所以每條邊都是對齊的 `AtomicU32`。`alloc_array` 的大小用 checked 算、不超過上限才配置，元素都在物件裡。
- 配置（`raw::alloc`、`alloc_array`）先整個清成 0、寫長度，才 `publish`：每條邊是 0（I2），長度和配置的大小相符；沒有 drop glue（`Finalizer::None`），所以 sweep 和 heap 的 drop 不讀它們的型別。執行時的編號都在 `MAX_TYPES` 以上，`downcast` 不會把它們當成 Rust 的型別。
- `unsafe` 的部分是 embedder 的契約：`write_edge`、`read_edge`、`write_bits`、`read_bits` 的 `at` 是物件的型別的邊（固定部分的，或長度以下的元素的），物件的格子檢查在 cage 裡；`write_edge` 的值是 none、`I31` 或同一個 heap 的物件（`compress` 檢查在 cage 裡），`write_bits` 的 bits 是 0、`I31` 的（奇數），或同一個 heap 的活的物件的 offset（`bits` 從 `Gc` 算的、這次操作裡 `read_bits` 讀到的），由 embedder 保證。兩者都經過 `Inner::write_raw_edge`：和 `Member::set` 一樣的兩種 barrier（I3、I10），`I31` 除外（下一點）。經過 `data` 的指標寫資料時不碰邊和長度：邊只經過 atomic 寫，marker 和 mutator 同時碰的還是只有邊（I4）；長度從配置之後不改，scavenge、marker 照它算大小和元素。
- 退休（`TypeRange::retire`，`unsafe`）的契約是之後（包括同時）不再配置這個型別，而且沒有它的物件到得了（root、weak root、邊、weak 的邊、id）。之後 GC 讀它的物件的型別只在：進行中的 marking 的快照、scavenge 經過死掉的 old 物件的 slot set 的格子 copy 它的年輕物件、heap verifier 走 nursery 的頁。退休記下每個活著的 heap 已經開始的 full GC 的次數加一，每個 heap 做完那麼多次（sweep 做完，`Inner::sweep_done` 的最後）或被 drop 之後才釋放 `Entry`：那次 full GC 在退休之後開始，它的快照到不了這些物件，開始時的 scavenge 把 nursery 清空（升上去的這些物件沒標到），它的 sweep 把它們都釋放了；之後沒有這個型別的物件，也不會再有（退休之後建的 heap 沒有，不用等）。計數在 full GC 開始時（`finish_sweeping` 之後）和 sweep 做完時、在 `Runtime` 的鎖底下改，退休在同一個鎖底下讀，所以和別的 thread 上的 GC 的先後是確定的：同時開始的那次算在退休之前，多等一次。
- 釋放之前那一格不給別的型別（`register_raw` 在鎖底下看到不是空的就拒絕），釋放和重新登記都在鎖底下；`raw_entry` 檢查退休的旗標，只抓照順序犯的錯，同時配置的由退休的契約排除。
- 一段編號（`TypeRange`）丟掉時，空的格子馬上還回去，退休的等釋放之後才還，還登記著的永遠不還（不是 UB：那些格子一直有效，只是浪費）。
- 執行時的型別的邊可能放著 31 位元的整數（`I31`，wasm 的 `i31ref`）：整數左移一位、最低位元設成 1，物件的 offset 都 4-aligned，所以 `raw::is_i31`（最低位元）分得開。GC 讀執行時的型別的邊的地方都先跳過它：marker、scavenge 掃物件、verifier 的三處。slot set 的格子不知道是哪一種物件的，記下之後可能被寫成 `I31`，所以 scavenge 的 `evacuate` 對每個值都跳過（Rust 的型別的邊只有 0 和物件的 offset，多一次位元的檢查）。寫邊的 barrier 新值是 `I31` 時不記 slot set、舊值是 `I31` 時不經過 SATB。Rust 的型別的邊（`Member`）不放 `I31`，它們的路不檢查。
- 不經過操作的入口（`alloc_bits`、`alloc_array_bits`、`write_bits_in` 要 `&mut Heap`，`read_bits_in` 要 `&Heap`）：呼叫時沒有操作在進行、也沒有 `Gc<'gc>` 活著；配置從不 GC（GC 該做時回 `GcDue`），所以呼叫中沒有東西搬動或釋放。配置和 `alloc` 共用 `allocate`（清零、寫長度、`publish`）。回的 bits 只是數字：safe 的程式拿它做不了什麼，用它都經過 `unsafe` 的入口。`read_bits_in`、`write_bits_in` 的契約同 `read_bits`、`write_bits`，物件改用 bits 給：`edge_in` 檢查格子在 cage 裡，寫經過 `Inner::write_raw_edge`。`from_bits` 的契約是 bits 是 0、`I31` 的，或一個活物件的 offset（frame 拿著的：交出 root 的 GC 留著它、寫回了新的 bits，操作裡不會 GC）。
- 交出 root 的 GC（`safepoint_with_roots`、`scavenge_with_roots`、`collect_with_roots`，`unsafe`）：契約是每個 root 是 0、`I31` 的或這個 heap 的活物件的 offset，第二次交的和第一次的一樣（數目不同就 panic；順序錯了寫回的是錯的數字，那是 embedder 的值，之後經過 `unsafe` 的入口才會用到）。它們也在 `&mut Heap` 上、不在操作裡，和 `collect` 一樣是安全點。root 的 bits 在 GC 期間放在 `Inner::frame_roots`：scavenge 把它們接在 root 表的格子後面，經過 `evacuate`（跳過 0 和 `I31`）更新，做完分回去（I3、I10）；marking 開始時經過 `root_offsets` 跳過 0 和 `I31` 再標，verifier 也從它們走；收尾不讀：frame 在開始之後拿到的物件是快照裡的（SATB 保住）、新配置的（黑的）或 young 的（old 的 marking 不判斷），frame 的格子被覆寫不經過 barrier，等於刪掉一個開始時就標了的 root（TLA+ 的模型裡拿掉刪 root 的 barrier 也不漏標）。
- frame 的狀態（`enter_frames`）和 safe 的程式的 soundness 無關：它只讓安全點少做 GC，守的是 embedder 的 bits（frame 裡安全點自己開始的 GC 看不到它們）；`collect`、`scavenge` 在 frame 裡 panic，免得 embedder 用錯。
## 找到並修正過的問題

- **Mark 把每個欄位都當成 `u32` 讀**（原型，Miri 抓到）：一般資料的欄位可能有 padding，讀到沒初始化的位元組是 UB。改成只讀 layout 裡標成 `Member` 的欄位（I4）。
- **配置先寫型別編號、再寫物件**（寫 I1 時審查找到）：init 寫到一半 panic 的話，下一次 sweep 會 finalize 沒寫完的物件。改成物件寫完才寫編號。
- **頁的 `Marks` 在 `Box` 裡，`MarkTable` 另外指向它**（2026-10-26，concurrent marking 加上 `MarkTable` 之後第一次在 Miri 下跑時抓到，Stacked Borrows）：頁的狀態改變時頁跟著搬，`Box` 的每次搬動都是一次 unique retag，`MarkTable` 從之前的參照拿到的指標就失效了，之後經過它讀 mark bit 是 UB（一般的 build 沒出事：`Box` 的 `noalias` 只用在函式參數上）。改成 `OwnedMarks`（`NonNull` 加上自己的 `Drop`，`aliasable` crate 的 `AliasableBox` 的做法），它的搬動不 retag。

## 怎麼驗證的

- **loom、shuttle**（`test.py models`）：loom 窮舉 worklist 的協定（交接、偷、結束的判斷，背景的 run 拿著最後一段時的「做完了」）、scavenge 的 forwarding（三個 thread 搶一個物件：只有一份 copy 贏、每個 thread 都拿到它）、concurrent marking 的發布（release 寫邊、relaxed 讀加 fence：看到邊就看到 mark bit 和寫好的物件）和 `cancel`（返回之後沒有 run）在 C11 記憶體模型下的 interleaving；shuttle 隨機排程整個 GC 在 pool 上的平行 marking、平行 scavenge，和 concurrent marking 加上 mutator 的寫入、刪 root、weak 的讀取，比對結果（ephemeron 沒有 shuttle 的測試：discovered 的交接只經過 mutex）。都數了自己真的碰到「worker 拿到工作」、「兩個 thread 搶同一個物件」，免得測了個寂寞。header 和 slot set 用的是 std 的 atomic，shuttle 不在它們上面切換 thread。
- **Miri**：`crates/elise/tests/it` 的全部和單元測試（其中的平行 marking 用真的 thread，Miri 查 data race），用 Miri 預設的 Stacked Borrows、開 `-Zmiri-strict-provenance`，cargo-nextest 平行跑（指令見 [`testing.md`](testing.md)）。Miri 底下不開 heap verifier、迴圈縮小。cage 在 Miri 底下是 16 MiB。Stacked Borrows 在大部分情況比 Tree Borrows 嚴；它報了我們認為是誤報的錯時，才用 Tree Borrows（`-Zmiri-tree-borrows`）確認。兩種都跑要多一倍以上的時間（Tree Borrows 比較慢）。
- **compile-fail**：`crates/elise/tests/ui/` 的 10 個案例，每個違反一條規則：`Gc` 帶出操作、存進外面的變數、在 heap 外建 `Member`、把 `Member` 搬出物件、物件自己實作 `Drop`、欄位放 `Box` / `Vec` / 參照、`External` 裝了不是 `Send` 的值、finalizer 碰到 GC 物件、借著 token 的 cell 時寫它、token 或 cell 離開它的 thread。
- **TLA+（和 JS engine 一起標記，`tla/unified/`）**：wrapper 存物件、物件帶 engine 的參照、engine 推進的 marking（I11）和 engine 那一側（Dijkstra、black allocation、收尾時重掃 root、scavenge 丟掉沒改過、只被物件拿著的 wrapper 並 reset 那一格）一起的模型，TLC 窮舉 2 個物件、2 個 JS 物件、每個物件 1 個欄位（也跑過 2 個欄位），不變式是收尾時兩邊到得了的都標過、沒有到得了的被釋放、有 wrapper 的物件不在 nursery、有 expando 的 wrapper 不被丟；拿掉每一部分各找得到反例。
- **TLA+（`tla/marking/`）**：concurrent marking 的協定（SATB、black allocation、marking 期間的 scavenge、marker 跳過 young、一個 ephemeron（沒有 value 時就是 weak 的邊）：讀取的 barrier、寫 value 的 SATB、marker 掃 holder 時標 value 或記下、固定點、收尾和 scavenge 的清除）的模型，TLC 窮舉兩種大小，不變式是收尾時到得了的（key 到得了時 value 也算）都標過、ephemeron 不指向釋放的物件、有指向 young 的邊的 old 物件都標過（`SlotsLive`：背景的 sweep 釋放的物件沒有 slot set 的記錄；拿掉開始前的 scavenge 或 black allocation 各找得到反例）；拿掉每一部分各找得到反例（拿掉刪 root 的 barrier 例外：root 在開始時全標了，之後加的 root 拿的是快照裡的、新配置的或 weak 讀出的）。weak root 沒有建模（收尾整張看，不靠記下）。模型和實作的對應靠審查。
- **Heap verifier**：測試和 fuzz 每次 GC 之後從 root 重走一遍，檢查 I1、I2、I3 和頁的帳（I8）；concurrent marking 收尾之後，標過但走不到的只能是開始時走得到的、weak 讀取時走得到的或 marking 期間配置的；weak 的邊和 weak root 指向標過的物件或 0（I2、I3）；兩種 slot set 的格子都在活的物件裡（I10）；scavenge 之後另外檢查到得了的 young 物件都在 nursery 現在的頁上、沒有 `FORWARDED`，每條 old 指向 young 的邊都在 slot set 上（weak 的在 weak 的 slot set 上，I10），weak 的邊和 weak root 不指向 from-space，nursery 的帳對得上。它不共用 marker 和 scavenger 的程式（自己的 DFS、算格子用除法不用乘法、從頁的開頭走 young 的物件），它們錯了它不會跟著錯。
- **Verus**：頁的 bitmap 搜尋（`crates/elise/src/bits.rs`）證明對任何長度的 bitmap，空區段剛好是所有最長的連續的 0、由低到高、互不相連（I1 的「配置只從空區段拿」靠它）。信任的是 Verus、Z3 和 `vstd` 的公理（例如 `u64::trailing_zeros` 的規格）。
- **fuzz**：對著不回收的參考模型跑隨機的操作，`cfg(fuzzing)` 下空的格子和頁對 ASan 毒化，讀到死物件時當場停下（讀出的值沒被用到時，`0xdb` 的毒值看不出來）。找到的輸入縮小後由 `cargo test` 和 Miri 重播。
- **Debug 的毒值**：sweep 把死掉的物件填上 `0xdb`，scavenge 把整頁 from-space 填上 `0xdb`，漏標或沒更新的邊在測試裡讀到的是毒值、不是舊的物件。
- **Mutation test**：弄壞 mark、sweep、finalizer、配置、`Member`、`Persistent`、`HeapVector`、derive、write barrier、scavenge、concurrent marking（barrier、black allocation、開始和收尾、取消）、weak（layout、記下、收尾和 scavenge 的清除、兩種 barrier）、ephemeron（layout、標 value 或記下、固定點、收尾和 scavenge 的清除、barrier）、id 表（young 的清單、清除和放掉塊、型別的比對、讀取的 barrier）和執行時的型別（layout 的每一條檢查、配置的清零和長度、marker、scavenge 和 verifier 走的邊、兩種 barrier、退休等的 GC 和它的計數、編號的保留和歸還、`I31` 的編碼和 marker、scavenge、verifier、barrier 跳過它、frame 裡的安全點不做 GC、交出 root 的 GC 讀和寫回 root、scavenge 和 marking 的開始用它們、bits 的配置什麼時候失敗、bits 的寫的 barrier），確認有測試失敗；結果記在進度的文件裡。

## 需要人工審查的地方

### 通用做法

改到下面的檔案時：每個 `unsafe` 有 `// SAFETY:`，說明它依賴哪一條不變式；新的 `unsafe` 由每週的 Miri（Stacked Borrows）涵蓋，要提早確認時手動跑一次；改了不變式要改這份文件。

### `elise-derive`：derive 產生的 layout 和 `__init`（G1）

- layout 的每個 `FieldInfo` 的 offset 是 `offset_of!`，kind 是欄位型別的 `HeapField::KIND`：不能自己判斷型別名稱。
- `__init` 對每個欄位呼叫一次 `InitSlot::field::<欄位型別>(offset_of!(..), ..)`，不能漏、不能重複。
- 拒絕泛型參數（同一個 static 的 `TypeSlot` 不能代表不同 layout 的型別）。
- 擋 `Drop` 的兩個 impl 還在。

### `crates/elise/src/data.rs`：`HeapData` 的 impl（G2）

新的 impl 只能是沒有指標、沒有 drop glue 的型別。`RefCell<T>` 可以：它的借用活不過操作，而 GC 不讀一般資料的欄位。

### `crates/elise/src/member.rs`、`crates/elise/src/persistent.rs`、`crates/elise/src/vector.rs`：邊和 root（I2）

- 每個從 offset 造 `Gc` 的地方，offset 都來自同一個 heap 的邊或 root，而且先做了執行期的檢查。
- `Gc::erase` 只換型別、不換指標；`Erased` 不能有欄位、不能實作 `GcObject`。轉回別的型別只有 `Gc::<Erased>::downcast`：讀物件 header 的第一個 word（型別的編號：物件發布前寫好、之後不變；scavenge 會搬物件、改 header 的旗標和物件的第一個 word，但只在操作之間，操作裡拿得到的 `Gc` 都是活的、沒被搬走的），和 `T` 的編號相同才換型別、不換指標，所以造出的 `Gc<'gc, T>` 指的真的是一個 `T`（同一個操作、同一個 heap）。
- `[Member<T>; N]`、`[WeakMember<T>; N]` 的 `KIND` 的 `N` 和 `__write` 寫的格數一樣；`WeakMember` 的 `KIND` 是 `Weak`（是 `Member` 的話它會留住物件，不會錯但不是 weak）。
- `WeakMember::get`、`WeakPersistent::get` 讀了值就經過 `Inner::read_weak`（weak 讀取的 barrier），讀到 0 回 `None`。
- `Persistent`、`WeakPersistent` 拿著的表的位址一直有效（I13）；`get` 比的是表的位址，所以別的 heap 的 root 一定被擋下。
- `RootTable` 的每個方法只在自己的 `with` 裡拿 `&mut Entries`，`with` 的 closure 都是那個方法自己的本體，不呼叫別人的程式、不 drop root、不把參照帶出去；要逐格改的（`update_entries`）先把格子拿出表，改完再放回去（I13）。
- `Mutation::by_id` 讀到 0 回 `None`，比對 header 的型別編號（`Erased` 除外）之後才經過讀取的 barrier、造 `Gc`。
- `EphemeronPair::value` 先經過 `key`（含讀取的 barrier），key 是 0 時回 `None`；`set` 和 init 在 key 是 0 時寫 0 的 value；value 經過 `write_edge`、key 經過 `write_ephemeron_key`。
- 寫邊只寫 `compress` 出來的值（或 0）。
- `HeapVector` 改長度時，`len` 以上的邊是 0。

### `crates/elise/src/bits.rs`：證明過的程式

`verus!` 裡的規格（`bit`、`run` 和 `ensures`）要說對：Verus 證的是程式符合規格，規格寫錯就證錯了東西。`#[verifier::external]`、`assume` 不該出現；`verus_only` 只用在 import 上（Verus 的規則：它在驗證時才打開）。

### `crates/elise/src/heap.rs`、`crates/elise/src/page.rs`：配置、mark、sweep、pool（I1、I3、I8、I9）

- header 和 `marks`、`finalizable` 的 bit 只在物件寫完後寫（`publish`）；`finalizable` 只在 sweep 和 heap 的 drop 清掉，每清一個 finalize 一次。
- 配置只從 sweep 找出的空區段（或新的頁）拿格子；GC 時每一級的 cursor 都清掉。
- Mark 只讀 `Member` 欄位（`[Member<T>; N]` 的 `N` 條）和陣列的 `capacity` 以下的邊。
- Sweep 在跑 finalizer 之前把 heap 整理好（頁的狀態、空區段、`available`、pool 都放回去）。
- discard 只對進 pool 之後過了一輪 GC 的頁；交給 worker 的頁是 `Discarding`，安全點拿回之前沒有人拿（I9）。
- `publish` 和快路的 `publish_bumped` 設 `finalizable` 的 bit 用一般的讀和寫（`set_finalizable_alone`），只在 heap 的 thread、在用的頁上（I6）；新增設 bit 的地方，如果在別的 thread 或交出去的頁上，要用 atomic 的 or（`set_finalizable`）。
- 快路配置的有 drop glue 的物件，頁上的計數（`finalizable_objects`、要在 heap 的 thread 上 finalize 的 `main_objects`）先記在它的 cursor 上（`Cursor::finalizable`、`main`），在 sweep 照計數交出頁之前、cursor 換頁之前（`refill`）和 verifier 數之前加回頁上（`flush_cursor_counts`）。`main_objects` 少算，有 `LocalExternal` 的頁就會交給 sweep 的 worker，`!Send` 的值在別的 thread 上 drop（I6）；新增讀這兩個計數的地方要先加回。
- 配置的量照 cursor 拿到的整段算（`refill` 的 `count_allocated`）：`used` 和 `allocated` 含 cursor 還沒用的尾巴，要精確的地方扣掉（`used_now`、門檻的 `allocated_past`，GC 清掉 cursor 時從 `used` 扣掉）；verifier 比的是 `used_now`。帳只關乎 GC 的時機和報給 engine 的大小，不關乎記憶體安全。執行時的型別的物件（`raw` 的 `allocate`）照大小拿 size class、經過同一個 `Inner::allocate` 和 `publish`，帳一樣。

### `crates/elise/src/heap/young.rs`：nursery、write barrier、scavenge（I1、I3、I10）

- 每個寫 `Member` 的地方都呼叫 `write_barrier`（`HeapVector::new` 寫自己的 `store` 也要：vector 可能因為 nursery 剛好滿了而在 old），每個寫 `WeakMember` 的地方都呼叫 `weak_barrier`；新增寫邊的 API 時也要。一開始就配在 old 的物件（`alloc_beside` 的擁有者在 old、`HeapVector` 的 store 跟著在 old 的 vector、有 JS 邊的型別、pretenure）和其他物件一樣經過 `InitSlot::field`、`MemberArray::write` 寫邊，所以它指向 young 的邊一樣記進 slot set（I10）；它們只是不進 nursery，配置走 `allocate_old`，marking 期間拿到的格子和其他配在 old 的一樣已經是黑的。
- copy 的 pair 的 value 經過 `update`（strong）、key 記進 copier 的清單；`update_weak` 清 key 時一起清 value。
- `update_weak` 也更新 id 表的 young 清單（`IdTable::update_young`）；`IdTable::insert` 在物件是 young 時列進清單。
- `update_weak` 在 copy 全部做完之後、from-space 的頁放掉之前跑：copy 的 weak 欄位、weak 的 slot set 和 weak root 指向 from-space 的，照 header 的去向改或清成 0；升上去的 copy 和 weak 的 slot set 裡還指向 young 的放回 weak 的 slot set。
- `forward` 先看 `FORWARDED`，copy 之前在新的位置拿到至少 `object_bytes` 的空間，copy 完才用 CAS 在舊的 header 寫上去向；輸的只退回在最上面的那份。升到 old 的由 heap 的 thread 在 `run` 之後經過 `publish`（`finalizable` bit），留在 nursery 的寫 `AGED`。from-space 的 header 只經過 64 位元的 atomic 碰。
- LAB 用不完的尾巴填空段，或在它是最後切出去的一段時退回；worker 的區段結束時還回那一頁。
- root 表的每一格、slot set 的每條邊、每個 copy（經過 worklist）的每條邊都經過 `update` 或 `update_slots`；`object_bytes` 和配置時湊的大小一樣（`MemberArray` 照它的 capacity，空段照 flags）。
- from-space 的頁在 finalizer 跑完之後才填毒值；`young_pages` 只在安全點改。

### `crates/elise/src/platform.rs`、`crates/elise/src/worklist.rs`、`crates/elise/src/heap/concurrent.rs`、`crates/elise/src/page.rs` 的 `MarkTable`：平行和 concurrent 的 marking（I3、I4、I7）

- `ThreadPool::run` 清掉 `state.job` 之前，`active` 是 0、`closed` 已經設上；`Platform` 的其他實作（之後的 V8 那個）同樣要保證全部做完才返回。
- `Marker` 和 `Base` 只讀 header 和邊、只寫 mark bit；`Base` 的 `Send`、`Sync` 靠：stop-the-world 的 marking 期間沒有人寫 heap，concurrent marking 期間 mutator 只 atomic 地寫邊、header 在發布前寫好（I4）。
- 每個寫邊的地方都經過 `Inner::write_edge`（marking 中先 `satb` 舊值、再 release 寫），寫 weak 的邊經過 `Inner::write_weak`（marking 中 release 寫，不用 `satb`）；新增寫邊的 API 時也要。配置時的 init 不經過，因為新的物件是黑的、marker 不掃它。
- marker 掃 pair 時 key 標過或是 young 才標 value，不然記成 discovered；收尾的 `Marker::finish` 反覆到 `resolve` 沒有新標的 value；`clear_weak` 清 discovered 剩下的 pair 時 key 和 value 一起清，也清 id 表（`IdTable::clear`）。
- marker 掃物件時把目標是 old、還沒標的 weak 格子記下，run 結束時合進 `weak_slots`；收尾的 `clear_weak` 在 job 取消、剩下的標完之後、sweep 之前跑，清掉記下的格子和 weak root 裡目標是 old、沒標的。
- marking 期間配置的物件在 mark bit 設上之後才可能被寫進邊：cursor 拿到區段時整段設上（`refill` 的 `black`）、large object 配置時設；scavenge 的 copier 也是（`Scavenging::black`）。收尾和 copier 還回區段時清掉沒用完的。
- 搬動物件、sweep、釋放頁和 cage 之前，job 已經 `cancel` 而且它的 run 都返回了（`scavenge_alone`、`end_marking`、`Heap::drop`）；`MarkTable` 的格子只在那之後拿掉。sweep 的 job 只讀它手上的頁的 mark bit；清 mark bit（下一輪 marking）和釋放 cage 之前先 `cancel` 它（`finish_sweeping`、`Heap::drop`）。
- `Platform::post_job` 的其他實作同樣要保證 `cancel` 返回之後沒有 run。
- worklist 的 `busy` 在每條路上都加減成對（拿到段時加、手上空了而且拿不到時減、panic 時由 `Busy` 減），否則呼叫的 thread 會一直等或提早結束。
- `max_concurrency` 不鎖 worklist（platform 在自己的鎖裡問它），只讀 `available`。
- engine 推進的 marking 的步驟（`Marker::step`）在 heap 的 thread 上，和 worker 一樣用 `run_background` 拿段、標、把手上的發布回去；heap 的 thread 就是 mutator，讀邊不用 fence（寫邊的是它自己）。它不算進 `active`：問 worker 是不是做完的只有 heap 的 thread，不在步驟中問。

### `crates/elise/src/js.rs`：engine 的 wrapper、JS 的邊（I11）

- `alloc_at` 在 driven 的 heap 裡，型別的 JS 的邊的清單不是空的就配置在 old；`engine_address` 檢查 driven 和型別。
- `Marker::hand` 先檢查位址在 cage 裡，再標、推進 worklist；只在 verifier 開著時記下交過來的（收尾時連它們到得了的一起算進 floating）。
- heap 在 engine 標記途中丟掉之後，engine 的 thread 還可能交物件過來（接 V8 的實驗在 `--stress-incremental-marking` 下找到：`hand` 設了已經釋放的頁上的 mark bit）：所以有 `Marker::retire`，marking 收尾和 heap 丟掉時關上 marker、等進行中的交接做完。
- marker 的 worklist 項目都是標過的物件的 offset；`scan` 把 JS 的邊的位址交給 local，不讀內容。
- `from_engine` 檢查位址在 cage 裡，經過讀取的 barrier 才造 `Gc`。
- `js_edges_of` 讀物件的 header 的型別編號，照型別表的 JS 的邊的 offset 給位址。
- `JsEdgeSink` 的 local 只在 marker 的 run 和步驟裡（`Found`）建和 drop。

### `crates/elise/src/types.rs`：型別表（I7）

表的格子在鎖底下寫。static 型別的格子只寫一次，指向註冊時 leak 的 `Entry`（型別的 layout 和它的邊的 offset 清單），永遠不釋放，所以 `TypeSlot::entry` 的 `&'static` 是對的；執行時的型別的 `Entry` 會釋放：`entry`、`info` 是 `unsafe fn`，每個呼叫的地方說明它讀的是 GC 還可能讀到的物件的 header（I14），`Runtime::reclaim` 是唯一釋放的地方，在鎖底下把格子換成 null 才交出去 drop。清單從 layout 的欄位照 `FieldKind::edges` 算出來，mark 只照它讀邊，所以它要和 layout 列的 `Member` 一樣（I4：不讀其他欄位）；weak 的清單照 `FieldKind::weak_edges`，和 layout 列的 `WeakMember` 一樣；ephemeron 的清單照 `FieldKind::ephemerons`（8 bytes 一個）。

### `crates/elise/src/raw.rs`：執行時的型別（I14）

- `check` 的每一條都對應 I14 的一個前提（mutation test 逐條拿掉過）；layout 加新的能力時先加檢查。
- `allocate` 先清零、寫長度，才 `publish`；`alloc_array` 的大小是 checked 的。
- `write_edge`、`write_bits` 只經過 `Inner::write_raw_edge`；`edge` 檢查格子在 cage 裡。GC 新增讀執行時的型別的邊的地方，要先用 `raw::is_i31` 跳過 `I31`；scavenge 經過 `evacuate` 的值都已經跳過。
- 不經過操作的入口（`alloc_bits`、`write_bits_in` 等）都在 `&mut Heap` 或 `&Heap` 上、從不 GC；`write_bits_in` 只經過 `Inner::write_raw_edge`，`edge_in` 檢查格子在 cage 裡。GC 新增讀 root 的地方（root 表之外），要一起讀 `Inner::frame_roots`（`root_offsets`、scavenge 的 root）；新增會在 frame 裡開始 GC 的路，要先拿到 frame 的 root。
- 退休等的計數：`gc_started` 在每個 full GC 的開始（`Inner::collect`、`start_marking_with` 的 `finish_sweeping` 之後），`gc_finished` 在 `sweep_done` 的最後。新增開始 full GC 的路、讓 sweep 或 heap 的 drop 讀到執行時的型別（例如給它們 drop glue）、或讓 GC 在別的時候讀死物件的 header 時，要重新看 I14 的論證。

### `crates/elise/src/heap/walk.rs`：heap snapshot 的走訪（I1、I2、I3、I14）

`Heap::walk` 交出 heap 拿著的每個物件（`HeapObject`）、它的邊的目標和 JS 的邊的參照，給 `Gc<'gc, Erased>`；`'gc` 是那次操作的，和 `mutate` 一樣不 GC。
- **哪些格子是物件**：拿 `&mut Heap`，先收完 sweep（`finish_sweeping`：worker 的 sweep 收手、結果套用，heap 的 thread 那種頁和死掉的 large object 掃完），所以沒有 `unswept` 的頁（`assert`），死物件的格子都回到空區段。這時 size class 的頁上，一格不在頁的空區段、也不在那一級的 cursor 還沒 bump 的那段（GC 時 cursor 都清掉，所以 cursor 的頁一定是那一級的頁），就是配置拿走、寫完了的物件（I1：配置只從空區段和 cursor 拿；safe 的 API 寫不出會 panic 的 init，cage 滿了在拿到格子之前就 panic，所以拿走的格子在操作繼續之前都寫完、發布了）。large object 的頁是一個物件（死的在 sweep 收完時都放掉了）。nursery 的頁從頭到頂是物件和 filler（I1，目前那一頁的頂在 cursor 裡），一個一個照 header 算大小走。header 的型別沒有登記的格子跳過（新的頁的 0、debug build 的 poison）：這只是多一層保險，不是論證的一部分。marking 進行中也一樣（那時 sweep 本來就做完了）。
- **讀的東西**：物件的 header、layout 列的邊（`u32`）和 JS 的邊的位址，都在操作裡、heap 的 thread 上讀；同時在跑的只有 concurrent marking 的 marker（只讀物件、寫 bitmap）和空頁的 discard（不碰有物件的頁），沒有別的 thread 寫物件。邊的目標照 I2 是 0 或 heap 拿著的物件（strong 的邊留住它；weak 的邊和 ephemeron 的目標死了在釋放之前清成 0），所以也是走訪的物件，`HeapObject::new` 讀得到它的 header。
- **執行時的型別（I14）**：走訪交出垃圾，所以它讀的型別可能是已經退休的。`types::info`、`entry` 的前提照樣成立：交出的是 heap 拿著、還沒放掉的物件；一個執行時的型別只在退休之後才開始的完整 GC 做完時放掉，那次 GC 開始時 nursery 裡沒有它的物件（開始前的 scavenge 清空 nursery，之後不再配置它），old 裡它的死物件由那次的 sweep 放掉，而走訪先收完 sweep。邊照 `Entry::raw_edges` 讀，`I31` 跳過，和 marker 一樣。
- **交出的物件在操作裡活著**：操作裡不 GC（I3）。沒有 marking 時，交出的垃圾寫進活的物件也沒事：下一次 GC 從那裡走到它。marking 進行中，交出的可能是 marking 會放掉的（開始時就是垃圾的、只有 weak 的邊走得到的），寫進一個掃過的物件之後 SATB 不會標它：所以 `Walk::new` 對每個物件照 weak 的讀取標上（`read_weak`，`from_engine` 的做法；young 的不標，scavenge 從邊走到它們），之後交出的都是這次 marking 會留下的（floating garbage，verifier 照 weak 的讀取記下）。邊的目標都是走訪的物件，也都標了。
- **JS 的邊**：交出 `&JsEdge`（I11：bytes 是 engine 的，heap 不讀寫），活到操作結束。
- 欄位的名字（`FieldInfo::name`）是 derive 在編譯時寫的 C 字串，和 layout 的安全無關。

### `crates/elise/src/token.rs`：token 的 cell（I12）

`LocalToken` 要一直不是 `Send`、`Sync`，`TokenCell` 不是 `Sync`；`get` 和 `get_mut` 回傳的參照的 lifetime 綁在 token 的借用上；`new` 的檢查和 `Drop` 的重設不能拿掉。新的方法不能給出比 token 的借用活得久的參照，也不能不經過 token 碰到值（`into_inner` 要擁有 cell）。

### `crates/elise/src/cage.rs`：保留、commit 和 discard（G3）

保留的大小是 cage 的兩倍，對齊之後的 cage 一定在保留區裡；commit 和 discard 的範圍檢查在 cage 裡。`Discarder` 是給 worker 的 discard：它只帶 cage 的起點，呼叫的人保證 cage 還在、範圍裡沒有在用的物件（I9）。discard 把範圍重新映射成沒有權限的匿名記憶體（Windows 是 `MEM_DECOMMIT`），之後要先 commit 才能讀寫。
