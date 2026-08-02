# エージェント向け指示

## agmsgでのレビュー依頼

agmsgで別エージェントに `jj diff` の確認を依頼する場合は、レビュー対象を再現できる識別子をメッセージ本文へ明記する。
単一のWorking Copyを確認させる場合は、そのchange IDとcommit IDを記載し、単に「現在のWorking Copy」や `@` とだけ書かない。
二つのrevision間の差分を確認させる場合は、始点と終点のcommit ID、および `jj diff --from <始点> --to <終点>` の形式による確認範囲を記載する。
終点のcommit IDまたはWorking Copyだけを範囲レビューの対象として示さない。
