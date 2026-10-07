---
name: verify
description: Anchor의 전체 검사 `cargo xtask verify`(생성 코드, API 대응표, 포맷, clippy, 봇 시험을 포함한 모든 시험, vendor 사본 시험, 문서)를 낮은 우선순위로 돌리고 결과를 보고한다. 사용자가 `/verify`로 시킬 때만 쓴다.
disable-model-invocation: true
---

# 전체 검사

1. 이미 도는 `cargo xtask verify`가 있으면 끝날 때까지 기다린다. 한 번에 하나만 돌린다.
2. 백그라운드로, 낮은 우선순위로 돌리고 출력은 파일로 받는다.
   - Windows: `cmd //c "start /low /b /wait cargo xtask verify" > target/verify.log 2>&1`
   - Linux: `nice -n 19 cargo xtask verify > target/verify.log 2>&1`
   - cargo가 PATH에 없으면 `$HOME/.cargo/bin`을 앞에 붙인다.
3. 끝나면 `ok  `/`FAIL`로 시작하는 줄과 마지막 줄(`PASS` 또는 `FAIL`)을 본다. `FAIL` 단계는 그 위의 출력에서 원인을 찾는다.
4. 사용자에게 마지막 줄, 실패한 단계와 원인, 걸린 시간을 쉬운 한국어로 짧게 보고한다. 이 변경 때문이 아닌 실패(환경, 다른 작업의 변경)는 그렇다고 따로 적는다.
5. 실패를 고쳤으면 고친 단계만 직접 다시 돌려 확인한다. verify 전체는 사용자가 다시 시킬 때 돌린다.

검사 항목과 시간은 `.claude/docs/testing.md`에 있다.
