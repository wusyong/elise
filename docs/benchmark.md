# Benchmark 的紀錄

elise 和 cppgc、V8 的 GC、Chrome 的 Oilpan 比較的數字。都是在 [machopo](https://github.com/wusyong/machopo)（用 elise 當 DOM 的 GC 的瀏覽器）裡量的：benchmark 的工具（`bench.py`）和原始資料在它的 repo，不在這裡。設計和各處對照 V8、cppgc 的理由見 [`design.md`](design.md)。

**怎麼讀**：每張表最後有「倍數」和「判定」。倍數一律是 **elise 的 ÷ 對照的**，而且已經換算成「成本」的方向，所以**小於 1 是 elise 比較好**。判定的標準：0.5 以下「好很多」、0.5 到 0.9「較好」、0.9 到 1.1「持平」（量測本身約有一成的起伏）、1.1 到 2「較差」、2 以上「差很多」。

## 來源和限制

- **第一到三節**：machopo 的 `bench.py gc`，2026-10-26，Apple M4 的 Mac（4 個 P-core、6 個 E-core，接電源），每項跑 5 次。原始資料是 machopo 的 `rfcs/0087-dom-gc/bench-5-apple-m4.json`（machopo `8edee3fcb1a3`；那時 elise 還是 machopo 裡的 `gc` crate）。那時 machopo 的 DOM 還在 cppgc 上，所以同一個 benchmark 兩邊都有實作：物件一樣大、連結一樣。
- **第四節**：machopo 的 `bench.py blink-perf --gc`，2026-11-13，Linux（AMD Ryzen AI 9 HX 370），machopo `161dae3` 對 Chrome for Testing 152，每頁各跑 3 次。
- **第一到三節是 10 月底的 elise**，而且 cppgc 的那一半之後已經從 machopo 刪掉，現在的程式沒辦法重跑這個對照。
- 表裡的 heap：`standalone` 是不接 V8 的 cppgc（當時 wasm 文件用的）；`isolate` 是接在 V8 的 isolate 上的 cppgc（當時 JS 文件用的，Chrome 也是這種）；`gc` 是 elise 只用一個 thread；`gc-parallel` 是 elise 用 worker（實際使用的設定）。
- **GC 時間只算 main thread 上的**。cppgc 另外有背景 thread 的時間（紀錄裡有，下面另列）；elise 的背景時間在這份資料裡記的是 0，我不確定是真的沒有還是當時沒有量，所以不拿來比。

## 一、elise 對 cppgc

### DOM 的工作：main thread 上的 GC 時間（ms）

build 建十萬個節點；churn 在一萬個節點的樹上反覆換掉子樹；replace 每回合把一千個節點整個換掉；list 是清單尾端加、頭拿掉。

| 工作 | cppgc standalone | cppgc isolate | elise 單 thread | elise | 倍數（對 isolate） | 判定 |
|---|---:|---:|---:|---:|---:|---|
| build | 8.07 | 7.49 | 4.04 | 2.17 | 0.29 | 好很多 |
| churn | 12.3 | 2.59 | 3.23 | 2.48 | 0.96 | 持平 |
| replace | 7.89 | 8.75 | 1.13 | 1.39 | 0.16 | 好很多 |
| list | 1.76 | 1.00 | 0.45 | 0.46 | 0.46 | 好很多 |

### 重播錄下來的頁面工作：main thread 上的 GC 時間（ms）

把真的頁面（blink_perf 的幾頁，wasm 文件的版本）的配置和釋放錄下來，在各個 heap 上重播。這組只有 standalone 的 cppgc（isolate 的在那台機器上會掉 trace 事件）。

| 頁 | cppgc standalone | elise 單 thread | elise | 倍數 | 判定 |
|---|---:|---:|---:|---:|---|
| create-element | 1001 | 7.06 | 7.18 | 0.007 | 好很多 |
| gc-forest | 1053 | 7.76 | 7.86 | 0.007 | 好很多 |
| gc-mini-tree | 1365 | 10.8 | 14.4 | 0.011 | 好很多 |
| gc-tree | 1908 | 1123 | 45.1 | 0.024 | 好很多 |
| innerHTML-setter | 1274 | 101 | 100 | 0.079 | 好很多 |

`insert-before` 不配置物件，兩邊的 GC 時間都是 0，沒有列。cppgc 在這幾頁另外有 590 到 1220 ms 的背景 thread 時間。

### 標記的速度（單 thread，每秒標幾百萬個物件；越高越好）

| 工作 | cppgc standalone | cppgc isolate | elise 單 thread | 倍數（成本，對 isolate） | 判定 |
|---|---:|---:|---:|---:|---|
| build | 25.6 | 43.5 | 46.1 | 0.94 | 持平 |
| chain-high | 185.1 | 193.0 | 269.6 | 0.72 | 較好 |
| tree-high | 159.3 | 157.3 | 216.4 | 0.73 | 較好 |
| graph-high | 62.3 | 62.2 | 62.4 | 1.00 | 持平 |

### 記憶體的高峰（peak used，MiB）

| 工作 | cppgc standalone | cppgc isolate | elise | 倍數（對 isolate） | 判定 |
|---|---:|---:|---:|---:|---|
| build | 32.9 | 32.9 | 30.5 | 0.93 | 持平 |
| churn | 5.3 | 11.4 | 12.4 | 1.09 | 持平 |
| replace | 1.8 | 8.6 | 5.5 | 0.64 | 較好 |
| list | 1.7 | 8.8 | 4.3 | 0.48 | 好很多 |
| chain-high | 49.5 | 51.4 | 30.2 | 0.59 | 較好 |
| tree-high | 49.5 | 50.8 | 30.2 | 0.59 | 較好 |
| graph-high | 57.4 | 60.5 | 45.5 | 0.75 | 較好 |
| create-element | 622 | 沒有 | 1.7 | 0.00（對 standalone） | 好很多 |
| gc-forest | 670 | 沒有 | 1.6 | 0.00（對 standalone） | 好很多 |
| gc-mini-tree | 670 | 沒有 | 1.5 | 0.00（對 standalone） | 好很多 |
| gc-tree | 672 | 沒有 | 351 | 0.52（對 standalone） | 較好 |
| innerHTML-setter | 737 | 沒有 | 14.1 | 0.02（對 standalone） | 好很多 |

小的工作（churn、replace、list）elise 對 standalone 的 cppgc 用得比較多（例如 replace 5.5 對 1.8 MiB），因為它有 nursery；對接在 isolate 上的 cppgc 則是差不多或較少。重播的頁差距很大（1.5 到 14 MiB 對 620 到 740 MiB），因為 cppgc 沒有新生代，垃圾要累積到整個 heap 的門檻才收。

## 二、elise 對 V8 自己的 GC

同樣形狀的物件圖，一邊配置在 elise、一邊寫成 JS 的 class 配置在 V8 的 heap。high 是留下 90% 的物件，low 是留下 5%。chain、tree、graph 是每個物件 1、2、4 條邊。

### main thread 上的 GC 時間（ms）

| 模式 | V8 | V8 單 thread | elise 單 thread | elise | 倍數（對 V8） | 判定 |
|---|---:|---:|---:|---:|---:|---|
| chain-high | 14.7 | 14.3 | 11.9 | 6.94 | 0.47 | 好很多 |
| tree-high | 18.4 | 18.1 | 13.5 | 7.67 | 0.42 | 好很多 |
| graph-high | 54.9 | 56.3 | 36.8 | 10.2 | 0.19 | 好很多 |
| chain-low | 1.40 | 1.43 | 1.12 | 0.94 | 0.67 | 較好 |
| tree-low | 1.56 | 1.62 | 1.28 | 1.05 | 0.67 | 較好 |
| graph-low | 2.91 | 2.83 | 3.71 | 2.28 | 0.79 | 較好 |

要注意兩件事：
- V8 另外有背景 thread 的時間（high 的模式 9 到 31 ms），沒有算在上表。
- 單 thread 的 elise 在 graph-low 上比 V8 慢（3.71 對 2.91 ms，1.27 倍）；用 worker 之後才比較快。

### 記憶體的高峰（MiB）

| 模式 | V8 | elise | 倍數 | 判定 |
|---|---:|---:|---:|---|
| chain-high | 22.1 | 30.2 | 1.37 | 較差 |
| tree-high | 25.2 | 30.2 | 1.20 | 較差 |
| graph-high | 32.3 | 45.5 | 1.41 | 較差 |
| chain-low | 2.8 | 4.3 | 1.53 | 較差 |
| tree-low | 3.0 | 4.6 | 1.55 | 較差 |
| graph-low | 4.5 | 4.7 | 1.02 | 持平 |

這一項 elise 輸：同樣的圖它用的記憶體是 V8 的 1.0 到 1.6 倍。當時的物件是照 cppgc 上的 DOM 節點的大小做的（每個約 344 bytes），JS 的 class 比較小，所以這不完全是 GC 的差別。

## 三、停頓時間

main thread 被 GC 停住的時間。這一節主要看絕對值：一個 frame 是 16.7 ms，所以幾毫秒以內都不會掉 frame。

### 最長的一次停頓（ms）

| 工作 | cppgc standalone | cppgc isolate | V8 | elise | 倍數（對 isolate，沒有就對 standalone） | 判定 |
|---|---:|---:|---:|---:|---:|---|
| build | 4.45 | 6.09 | 沒有 | 0.47 | 0.08 | 好很多 |
| churn | 2.64 | 0.91 | 沒有 | 0.56 | 0.62 | 較好 |
| replace | 0.10 | 0.56 | 沒有 | 0.06 | 0.11 | 好很多 |
| list | 0.17 | 0.66 | 沒有 | 0.08 | 0.13 | 好很多 |
| chain-high | 5.40 | 5.66 | 7.87 | 2.01 | 0.36 | 好很多 |
| tree-high | 6.30 | 7.53 | 7.40 | 2.20 | 0.29 | 好很多 |
| graph-high | 12.4 | 6.63 | 23.8 | 3.02 | 0.46 | 好很多 |
| chain-low | 0.59 | 1.36 | 0.14 | 0.10 | 0.08 | 好很多 |
| tree-low | 0.62 | 1.45 | 0.18 | 0.10 | 0.07 | 好很多 |
| graph-low | 0.98 | 1.70 | 0.20 | 0.21 | 0.13 | 好很多 |
| create-element | 42.0 | 沒有 | 沒有 | 4.41 | 0.11 | 好很多 |
| gc-forest | 43.6 | 沒有 | 沒有 | 2.45 | 0.06 | 好很多 |
| gc-mini-tree | 60.6 | 沒有 | 沒有 | 0.04 | 0.00 | 好很多 |
| gc-tree | 59.6 | 沒有 | 沒有 | 3.28 | 0.06 | 好很多 |
| innerHTML-setter | 48.3 | 沒有 | 沒有 | 6.05 | 0.13 | 好很多 |

- `innerHTML-setter` 的重播 elise 有一次 6.05 ms 的停頓，是 1465 次 marking 裡唯一的一次（p95 是 0.18 ms），原因當時沒有查。
- 重播的頁上 cppgc 最長的停頓是 42 到 61 ms，elise 是 0.04 到 6 ms。

### elise 的 scavenge（新生代的回收）最長的一次（ms）

| 工作 | V8 的 scavenge | elise | 倍數 | 判定 |
|---|---:|---:|---:|---|
| chain-high | 2.52 | 2.01 | 0.80 | 較好 |
| tree-high | 4.59 | 2.20 | 0.48 | 好很多 |
| graph-high | 8.51 | 3.02 | 0.35 | 好很多 |
| chain-low | 0.14 | 0.08 | 0.52 | 較好 |
| tree-low | 0.18 | 0.08 | 0.47 | 好很多 |
| graph-low | 0.20 | 0.16 | 0.82 | 較好 |

cppgc 沒有新生代，所以沒有這一項可以比。

## 四、machopo（elise）對 Chrome（Oilpan），真的頁面

這一節不是單純比兩個 GC：兩邊的 DOM、V8 的設定、排程都不同。數字是 runner 計時的區間裡的 GC 時間，machopo / Chrome。

### main thread 上全部的 GC（ms）

| 頁 | Chrome | machopo | 倍數 | 判定 |
|---|---:|---:|---:|---|
| create-element | 2980 | 1099 | 0.37 | 好很多 |
| gc-tree | 343 | 276 | 0.80 | 較好 |
| gc-forest | 317 | 232 | 0.73 | 較好 |
| gc-mini-tree | 310 | 251 | 0.81 | 較好 |

### DOM heap 的總 CPU：main thread 加其他 thread（ms）

| 頁 | Chrome | machopo | 倍數 | 判定 |
|---|---:|---:|---:|---|
| create-element | 4363 | 105 | 0.02 | 好很多 |
| gc-tree | 595 | 191 | 0.32 | 好很多 |
| gc-forest | 343 | 39.9 | 0.12 | 好很多 |
| gc-mini-tree | 504 | 42.5 | 0.08 | 好很多 |

這是 elise 對 Oilpan 最直接的一項：兩邊各自花在 DOM heap 上的 CPU，不含 V8 的部分。

### V8 那一側在 main thread 上的 GC（ms）

| 頁 | Chrome | machopo | 倍數 | 判定 |
|---|---:|---:|---:|---|
| create-element | 1697 | 1048 | 0.62 | 較好 |
| gc-tree | 220 | 269 | 1.22 | 較差 |
| gc-forest | 209 | 223 | 1.07 | 持平 |
| gc-mini-tree | 189 | 243 | 1.29 | 較差 |

`gc-tree` 和 `gc-mini-tree` machopo 比較差，`gc-forest` 持平。這不是 elise 本身：Chrome 在頁面載入的前幾秒會告訴 V8 晚一點開始 marking，machopo 沒有，所以 full GC 的次數比較多（見下表）。`create-element` 上 machopo 在區間裡沒有 scavenge，V8 那一側因此比較少。

### 最長的一次停頓（ms）

| 頁 | Chrome | machopo | 倍數 | 判定 |
|---|---:|---:|---:|---|
| create-element | 4.97 | 4.85 | 0.98 | 持平 |
| gc-tree | 41.0 | 23.6 | 0.58 | 較好 |
| gc-forest | 15.8 | 9.82 | 0.62 | 較好 |
| gc-mini-tree | 10.7 | 9.88 | 0.92 | 持平 |

### V8 的 GC 次數（每次執行）

| 頁 | full GC：Chrome | full GC：machopo | scavenge：Chrome | scavenge：machopo |
|---|---:|---:|---:|---:|
| create-element | 511 | 437 | 77 | 0 |
| gc-tree | 7 | 9 | 69 | 40 |
| gc-forest | 24 | 63 | 80 | 42 |
| gc-mini-tree | 48 | 57 | 63 | 44 |

### 這幾頁整頁的成績

GC 比較少不代表整頁比較快：

| 頁 | machopo 對 Chrome |
|---|---|
| create-element | 2209 對 3553 runs/s |
| gc-tree | 137 對 133 ms |
| gc-forest | 99.8 對 59.9 ms |
| gc-mini-tree | 117 對 96.1 ms |

除了 `gc-tree` 持平之外，machopo 整頁都比較慢，慢在建節點和 binding 的呼叫，不在 GC。

### 另外兩個只有 2026-11-10 量過的數字

- `parser/innerHTML-setter`：main thread 上的 GC 429 對 2945 ms（0.15，好很多）；Chrome 最長的停頓是收尾的 `CppGC.SweepFinish` 82 ms，machopo 0.8 ms。
- `gc-forest` 的記憶體高峰（嵌入端的 heap）：167 對 302 MiB（0.55，較好）。這是一次暫時的量測。

## 總結

| 面向 | 對 cppgc | 對 V8 的 GC |
|---|---|---|
| GC 時間 | 好很多到較好（存活率低的工作差距最大） | 好很多到較好（用 worker 時） |
| 標記的速度 | 較好到持平（全部活著的圖持平） | 沒有直接比 |
| 最長的停頓 | 好很多到較好 | 好很多到較好 |
| 記憶體的高峰 | 大的工作好很多；小的工作對 standalone 較差、對 isolate 持平 | 較差到持平 |

沒有量過的：配置的成本（Speedometer 的取樣裡 machopo 有 9.7 ms 花在 DOM heap 的配置上，Chrome 的配置 inline 在 Blink 裡分不出來）、現在這版 elise 對 cppgc 的直接對照。
