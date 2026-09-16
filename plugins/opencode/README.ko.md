# rtrt-agent

`rtrt-agent`는 RTRT의 OpenCode 통합입니다. 지원되는 hook 사이에서 상위 session,
agent, worktree, invocation 및 permission broker 문맥을 보존하고 로컬 대시보드를
백그라운드에서 사용할 수 있게 유지합니다.

## 설치

```sh
npm install rtrt-agent@0.1.3
```

`opencode.json`의 단수형 `plugin` 배열에 패키지를 등록합니다.

```json
{
  "plugin": ["rtrt-agent@0.1.3"]
}
```

OpenCode는 이름 있는 `RtrtProvenance` export를 로드합니다. 이 패키지는 의도적으로
default export를 제공하지 않습니다. 선택 사항인 RTRT TUI statusline은 별도 소스
통합으로 유지되며 이 패키지에서 export하거나 pack하지 않습니다.

## 대시보드

플러그인을 로드하면 `rtrt-agent`는 `127.0.0.1:7311`에서 실행되는 사용자별 detached
`rtrt-dashboard` process를 예약합니다. 시작 과정은 대시보드 준비를 기다리지 않으며
브라우저를 자동으로 열지 않습니다. 정확히 같은 버전의 플랫폼별 optional npm 패키지가
실행 파일을 제공하므로 미리 설치된 `rtrt`, `PATH` 검색, install script 또는 project
build를 사용하지 않습니다.

필요할 때만 브라우저를 엽니다.

```sh
rtrt-dashboard-open
```

에이전트는 인자가 없는 `rtrt_dashboard_open` 도구로 같은 명시적 동작을 수행할 수
있습니다. 두 경로 모두 60초 fragment bootstrap을 사용하고 고정 상태만 반환하므로
credential이 prompt나 command template에 들어가지 않습니다. 실행 파일 또는 private
state가 안전하지 않거나 없으면 시작만 fail-soft로 중단되고 OpenCode는 정상적으로
계속됩니다. 기존 `~/.rtrt` 데이터는 보존합니다.

`rtrt setup --agent opencode --apply`는 전체 RTRT 통합을 관리하지만 npm 설치는 수행하지
않으며, 설정된 패키지는 OpenCode가 시작할 때 설치합니다. Setup은 먼저 정확한
`rtrt-agent@0.1.3` 등록과 모든 대체 관리 asset을 기록합니다. 이 기록이 성공한 뒤에만
인식된 legacy RTRT plugin을 제거하며, 이전 단계가 실패하면 legacy runtime을 보존합니다.
Setup은 `rtrt-agent`의 정확히 일치하는 bare, pinned, ranged, tuple, object 형식만 소유하고
정확한 `"rtrt-agent@0.1.3"` string 하나로 정규화합니다. 이전의 미게시 `rtrt`와 초안
`rtrt-opencode` 패키지 spec은 외부 값이며 순서를 유지합니다. 위 직접 등록과
`opencode plugin rtrt-agent@0.1.3 --global`은 계속 유효합니다.

관리 agent 소유권 상태는 다음 순서의 첫 비어 있지 않은 root에서 읽습니다.
`$OPENCODE_CONFIG_DIR`, `$XDG_CONFIG_HOME/opencode`, `~/.config/opencode`. 공존은 OMO
4.19.4를 대상으로 CI에서 검사하고 OpenCode 1.18.29에서 직접 검증했으며, 어느 쪽도
미래 버전 지원을 보장하지 않습니다. 통합 RTRT 릴리스 워크플로는 정확한 버전의
대시보드 플랫폼 패키지 5개를 `rtrt-agent@0.1.3`보다 먼저 게시한 뒤 일치하는 Rust
릴리스 artifact를 게시합니다.

## 라이선스

MIT
