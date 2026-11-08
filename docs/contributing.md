# 參與開發

## 和 machopo 的關係

elise 從 [machopo](https://gitlab.com/kunlabs/kun) 的 `gc` crate 分出來。在 machopo 換過來用它之前，machopo 仍然用自己的 `crates/gc`，兩邊的改動要手動同步：除了 crate 的名字（`gc` → `elise`、`gc_macros` → `elise-derive`）、指向 machopo 的路徑和指令，和 `cargo fmt` 排的格式，程式碼和測試一樣。machopo 換過來時直接依賴 `elise`，不改名。

## 規則

- **`unsafe` 只在 `elise`**：workspace 的 `unsafe_code = "deny"` 擋掉其他地方（`elise-derive` 產生的程式碼也受它管），只有 `crates/elise/src/lib.rs` 和它的整合測試打開。每一處都有 `// SAFETY:` 說明；改了論證要更新 [`soundness.md`](soundness.md)。用 elise 的程式碼不寫 `unsafe`。
- **整合測試是 `tests/it/` 的模組**（加一個檔案、在 `main.rs` 加一行 `mod`），不直接放在 `tests/` 底下：那會多一個要重新 link 的 binary。trybuild 的 `tests/compile_fail.rs` 例外。
- **文字檔一律 LF**（`.gitattributes`）。`crates/elise/fuzz/regressions/` 的 `.bin` 照原樣保存。
- **新的行為要有測試**，並且弄壞實作確認有測試失敗（mutation test）。

### 整數運算寫明溢位時怎麼辦

workspace 打開 clippy 的 `arithmetic_side_effects`、`cast_possible_truncation`、`cast_sign_loss`、`cast_possible_wrap`：`+`、`-`、`*`、`/`、`%`、`<<` 這些運算子和可能丟資料的 `as` 都要改寫成寫明溢位時怎麼辦的形式。debug 下溢位會 panic，release 下悄悄 wrap，兩者都不是想要的。

| 用途 | 寫法 |
| --- | --- |
| 大小、配置、索引的計算 | `checked_*`，失敗時回錯誤或 panic（cage 滿了） |
| 統計、計數器 | `saturating_*`，或 wrap 也無妨時 `wrapping_*` |
| hash、亂數 | `wrapping_*` |
| 整數轉整數 | 放得下的用 `From`；可能放不下的用 `try_from`，再決定錯了怎麼辦 |
| 證明過不會溢位 | 先試著改寫；改寫不了的 `#[expect(..., reason)]` |

- **例外**一律 `#[expect(lint, reason = "...")]`，不用 `#[allow]`：程式改了、那裡不再觸發時，`expect` 會報沒有觸發。理由寫為什麼不會溢位，而且讀的人能就近驗證，例如 `reason = "i < items.len(), so i + 1 <= len"`。屬性不能放在單一運算式上，例外最小是一個 `let`、statement 或函式；能改寫（iterator、`checked_sub` 的 `?`）就改寫。證明過不會溢位的不寫 `checked_*().expect(...)` 代替：它在 release 多一個分支，GC 的熱迴圈付不起。
- **測試不套用**：每個 lib 的根有 `#![cfg_attr(test, allow(..., reason = "tests"))]`，每個整合測試的根有 `#![allow(..., reason = "tests")]`。
- **atomic 整數的 `fetch_add`** 由 `clippy.toml` 的 `disallowed-methods` 擋：它是方法呼叫，lint 看不到它悄悄 wrap。單純統計、wrap 也無妨的寫 `#[expect(clippy::disallowed_methods, reason = "...")]`。
- **`verus!` 裡的程式**照樣受這些規則管，`#[expect]` 的理由寫 Verus 證了什麼。
- **lint 看不到的**要自己注意：`pow`、`abs`、`Iterator::sum`、`product`、atomic 的 `fetch_sub`、`Duration::from_secs_f64`。
