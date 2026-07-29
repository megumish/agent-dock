# 調査ノート（非規範）

[DESIGN.md](../DESIGN.md) の Decision Register を補助する、調査結果とその時点の傾きの置き場である。

- **規範性はない**: ここの候補・傾きは ADR を拘束しない。**列挙は網羅ではなく、ADR は列挙外の案を採用してよい。**
- **スナップショットである**: 記載は日付時点の事実・観察であり陳腐化しうる。ADR 起草時は環境 inventory と証拠を取り直す。
- 既存の採用候補が見当たらない決定点は「なし＋理由」を記す。
- 記述の拘束力の定義は DESIGN「読み方」を参照。

## D-01 実行基盤（2026-07-29）

- 環境 inventory: claude (Claude Code 2.1.214) / codex-cli 0.144.4 / gemini-cli 0.47.0 導入済み。
- A（headless 束ね）の詳細: Claude Code は `claude -p --output-format json` が usage・総コスト・所要時間等を返し hooks・許可機構を持つ。codex は `codex exec`＋JSON イベント。gemini は headless＋JSON。
- C（自前ハーネス）の候補: Claude Agent SDK・各社 SDK・LiteLLM 等。
- 傾き: A 主軸＋必要箇所に C。

## D-02 イベント記録（2026-07-29）

- 参考標準: CloudEvents・OpenTelemetry trace/span・W3C Trace Context。
- ストレージ候補: SQLite（導入済。トリガで更新禁止を強制する案）/ JSONL 追記＋派生インデックス / DuckDB（未導入）。
- 傾き: 封筒6項目＋型別拡張、本文は参照＋ハッシュ、v0 ストアは SQLite か JSONL。

## D-03 タスク分類（2026-07-29）

- 既存標準: なし — 汎用の作業分類標準は調査の範囲で個人のエージェント作業に適合しない（ADR で再確認）。
- 傾き: 粗い6種別（設計/実装/大量編集/調査/レビュー/運用）＋自由タグ。

## D-04 役割と適性要件（2026-07-29）

- 要件表現の個別候補: YAML＋JSON Schema 検証 / OPA/Rego・Cedar（過剰の懸念つき）/ コード内定義。
- 傾き: 宣言的設定ファイル。

## D-05 采配方式（2026-07-29）

- 文脈付きバンディットの既存 OSS: MABWiser・Vowpal Wabbit。
- 傾き: A（ルール＋スコアカード）の骨格に B（LLM 采配）を載せる。C は探索配分で調査継続。

## D-06 フィードバック UX（2026-07-29）

- 明示の実現候補: 完了時ワンタップ/一文字・`/rate` 相当。
- 既存部品: Claude Code hooks・TUI 部品（gum 等）。
- 傾き: 受動優先＋明示は監査対象に絞る。「明示は1アクション以内」は上限の候補（計測で見直す）。

## D-07 リプレイ方式（2026-07-29）

- 事実: このリポジトリは jj（colocated git）で管理されている。
- VCS アダプタの個別候補: jj workspace / git worktree（jj 管理との両立は要検証）/ 隔離コピー / コンテナ（Docker・Podman）。
- 傾き: なし（jj workspace と隔離コピーを実測比較してから）。

## D-08 外部取り込み（2026-07-29）

- 価格: LiteLLM 価格 DB の定期取り込み / 手動更新。
- prior の参考情報源: HELM・公開リーダーボード・モデルカード。
- シグナル: 手動貼り付け → RSS リーダ（miniflux 等）・changelog 監視（後段）。
- 傾き: 手動優先で開始、価格のみ半自動を早める。

## D-09 資格ゲート（2026-07-29）

- B（ホスト機構の併用）の個別機構: ホストの permissions/hooks・資格保有プロファイルのみの設定生成。
- OS サンドボックス候補: firejail 等（過剰の懸念つき）。
- 傾き: A 原則＋B 補助。

## D-10 鮮度・再審査（2026-07-29）

- ドリフト検知の既存 OSS: river（ADWIN・Page-Hinkley）・evidently。
- 傾き: なし（データを見てから）。

## D-11 リスク分類と探索予算（2026-07-29）

- 方針評価の既存物: OPA/Rego・Cedar（軽量自作と比較）。
- 傾き: チェックリスト＋定額予算。

## D-12 代行監査（2026-07-29）

- 一致統計の既存 OSS: statsmodels / scikit-learn（κ係数）・krippendorff。
- 傾き: 逓減サンプリング＋次元定義された独立性。

## D-13 実行プロファイルの同一性（2026-07-29）

- 参考: OCI イメージ設定・ロックファイル慣行。
- 傾き: 正規化ハッシュ＋可読名＋レシート併記。

## D-14 作業とイベントの紐付け（2026-07-29）

- 参考標準: OpenTelemetry trace/span・W3C Trace Context・CloudEvents。
- 傾き: DAG 許容の階層＋OTel 風因果参照。

## D-15 ユーザー権限・方針（2026-07-29）

- 汎用ポリシー言語の個別候補: OPA/Rego・Cedar。
- 傾き: 宣言的設定ファイル＋run 開始時スナップショット＋署名つき単発許可。

## D-16 評価契約（2026-07-29）

- 統計: statsmodels（非劣性の区間推定）。
- 事前登録: preregistration 慣行を参考に軽量自作で足りる見込み。
- 傾き: baseline 2種併記＋frozen cohort/future window 併用＋代理指標3種。

## D-17 射影カタログと較正（2026-07-29）

- 較正の既存 OSS: scikit-learn（calibration）・MAPIE（conformal prediction）。
- 傾き: 最小形は代表3軸のカタログ＋有効標本数＋区間表示。

## D-18 アダプタ契約（2026-07-29）

- 記述形式の候補: JSON Schema。参考: OTel semantic conventions・CloudEvents 属性。
- 傾き: JSON Schema 契約・必須は最小集合。

## D-19 スキーマ・射影の進化（2026-07-29）

- スキーマ検証: JSON Schema。移行ツール: dbmate・sqlite 系（ストレージ選定に従属）。参考: CloudEvents のバージョニング指針。
- 傾き: 入力識別＋版の記録を最小形に（消去可能性を織り込む）。

## D-20 リポジトリ境界（2026-07-29）

- 傾き: C（分割 — ドック固有の契約・アダプタ・入口・非秘密の既定値は本リポジトリ、マシンのブートストラップ・汎用設定・秘密は dotfiles）。

## D-21 境界執行（2026-07-29）

- 秘密パターン検査のルール流用元: gitleaks / trufflehog。
- 傾き: 許可リスト（egress policy）＋tool mediation の併用＋方針スナップショット。

## D-22 資格ライフサイクル（2026-07-29）

- FSM ライブラリ: python-statemachine・XState 等（過剰の懸念つき — 本質は遷移の宣言と正典記録）。
- 傾き: 遷移イベント方式＋任用・再任のみ承認必須。

## D-23 監査再構成契約（2026-07-29）

- 既存物: なし — 監査再構成は本システム固有の契約であり汎用 OSS は存在しない（event-sourcing の監査パターンを設計の参考にする）。
- 傾き: 決定イベントに参照埋め込み。

## D-24 障害・イベント完全性契約（2026-07-29）

- 既存物: 配送意味論の実装はメッセージキュー系 OSS に存在するが、ローカル単一利用者前提では過剰の懸念。outbox パターン等の分散システム慣行を参考に自作が軽量の見込み。
- 傾き: at-least-once＋レシート突き合わせ＋outbox 参考。
