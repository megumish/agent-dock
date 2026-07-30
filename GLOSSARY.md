# 用語対訳集

この文書は、[VISION.md](./VISION.md) が定める用語に対応する英語表記を定める。
実装のコード、識別子、コミットメッセージで用語を英語で書くときは、この対訳を使う。
用語の意味はこの文書では定義せず、[VISION.md](./VISION.md) の定義に従う。

対訳は、[DESIGN.md](./DESIGN.md) が識別子として既に使っている英語（運用モードの `shadow`、資格状態の `qualified` など）と揃えている。
選定に判断が入った語には、備考欄に理由を示す。

<!-- markdownlint-disable MD013 -->

## 基本概念

| 日本語 | 英語 | 備考 |
| --- | --- | --- |
| ドック | dock | |
| 乗組員 | crew member | ドック（船渠）の比喩に合わせる。集合は crew。 |
| 実行プロファイル | execution profile | |
| 構成 | configuration | |
| 実行基盤 | execution platform | 「ハーネス」より広い語を選んだ日本語側の経緯に合わせ、harness を使わない。harness はリプレイハーネス（replay harness）に限る。 |
| 評価 | evaluation | |
| 振り分け | routing | |
| 采配 | dispatch | 振り分け（routing）の中の一回ごとの委任判断。DESIGN の采配ルータは dispatch router。 |
| 評価イベント | evaluation event | |
| 遅延イベント | late event | ストリーム処理で境界より後に届くイベントを指す慣用語に合わせる。 |
| エスカレーション | escalation | |
| 裁定 | adjudication | ユーザーの裁定は user adjudication。DESIGN の裁定ガードは adjudication guard。 |
| ローカル | local | |

## 評価イベントの内訳

| 日本語 | 英語 | 備考 |
| --- | --- | --- |
| 実測 | measurement | 実行の実測は execution measurement、実タスクの実測は real-task measurement。 |
| 実支出 | actual spend | |
| 決定 | decision | |
| フィードバック | feedback | |
| 帰結 | outcome | |
| 受け入れ | acceptance | |
| 手戻り | rework | |
| 破棄 | discard | |
| 来歴 | provenance | |

## 射影とスコア

| 日本語 | 英語 | 備考 |
| --- | --- | --- |
| 射影 | projection | |
| スコア | score | |
| 評価軸 | evaluation axis | |
| 鮮度 | freshness | 鮮度に応じた重みづけは recency weighting。 |
| 不確実性 | uncertainty | |
| 適用範囲 | scope | |
| 意思決定の確かさ | decision calibration | 期待した帰結と観測された帰結の整合度を測るため、正答率（accuracy）でなく較正の語を充てる。 |
| 作業速度 | turnaround time | 指示から受け入れ可能な成果までの実測時間を指すため、speed でなく所要時間の慣用語を充てる。 |
| 成果あたり実コスト | actual cost per accepted outcome | |

## 教師信号と評価の代行

| 日本語 | 英語 | 備考 |
| --- | --- | --- |
| 教師信号 | ground truth | VISION 本文に併記済み。 |
| 観測事実 | observed fact | |
| 評価の代行 | delegated evaluation | 運用モードの `delegated`（委任実行）と語幹を共有するため、単独の delegation とは書かず複合語で書き分ける。 |
| 観測状態 | shadow state | DESIGN の運用モード `shadow` と同じ語で揃える。観測出力は shadow output。 |
| 一致実績 | agreement track record | |
| 一致率 | agreement rate | |
| 判断の優先 | user-judgment precedence | |
| 常時監査 | continuous audit | |
| 領域の限定 | domain restriction | |
| 独立性 | independence | |

## 証拠と審査

| 日本語 | 英語 | 備考 |
| --- | --- | --- |
| 証拠 | evidence | |
| 外部ベンチマーク | external benchmark | |
| 外部事前情報 | external prior | prior は VISION 本文に併記済み。 |
| 外部シグナル | external signal | |
| リプレイ | replay | |
| 自前リプレイ | self-replay | |
| 定点観測 | drift monitoring | 直訳の fixed-point observation は数学の不動点を連想させるため、目的で名付ける。 |
| ドリフト | drift | |
| 初期審査 | initial review | |
| 再審査 | requalification | 資格（qualification）の再判定であるため、re-review でなく資格の語に接続する。 |

## 役割と資格

| 日本語 | 英語 | 備考 |
| --- | --- | --- |
| 役割 | role | |
| 采配役 | dispatcher | |
| 設計役 | architect | 役割名としての慣用に合わせる（文書や設計行為の設計は design）。 |
| 実装役 | implementer | |
| 検証役 | verifier | |
| 適性 | aptitude | |
| 適性要件 | aptitude requirements | |
| 資格 | qualification | 状態は DESIGN の `candidate`、`qualified`、`on-hold`、`revoked`、`expired` に対応する。 |
| 付与、保留、取り消し、失効 | grant, hold, revocation, expiration | 保留中の状態は on-hold、取り消し後は revoked、失効後は expired。 |

## 采配の循環と自律判断

| 日本語 | 英語 | 備考 |
| --- | --- | --- |
| 活用 | exploitation | 探索（exploration）とあわせて、バンディット問題の慣用対に合わせる。 |
| 探索 | exploration | 探索予算は exploration budget。 |
| 低リスク | low-risk | |
| 再審査トリガー | requalification trigger | |
| 自律判断の優先順位 | priority order for autonomous judgment | 第 n 層は layer n。 |
| 不可逆な帰結 | irreversible consequence | |
| 単発許可 | one-time permission | |
| ユーザー方針 | user policy | |
| 保持境界 | retention boundary | |

## 成功基準

| 日本語 | 英語 |
| --- | --- |
| 振り分けの成功 | routing success |
| 評価の成功 | evaluation success |
| 持続の成功 | sustainability success |

<!-- markdownlint-enable MD013 -->
