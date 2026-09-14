# rtrt-agent

`rtrt-agent`는 RTRT의 OpenCode provenance 플러그인입니다. 지원되는 OpenCode
hook 사이에서 상위 session, agent, worktree, invocation 및 permission broker 문맥을
보존하며 broker는 loopback에서만 실행됩니다.

## 설치

```sh
npm install rtrt-agent@0.1.2
```

`opencode.json`의 단수형 `plugin` 배열에 패키지를 등록합니다.

```json
{
  "plugin": ["rtrt-agent@0.1.2"]
}
```

OpenCode는 이름 있는 `RtrtProvenance` export를 로드합니다. 이 패키지는 의도적으로
default export를 제공하지 않습니다. 선택 사항인 RTRT TUI statusline은 별도 소스
통합으로 유지되며 이 패키지에서 export하거나 pack하지 않습니다.

`rtrt setup --agent opencode --apply`는 전체 RTRT 통합을 관리하지만 npm 설치는 수행하지
않으며, 설정된 패키지는 OpenCode가 시작할 때 설치합니다. Setup은 먼저 정확한
`rtrt-agent@0.1.2` 등록과 모든 대체 관리 asset을 기록합니다. 이 기록이 성공한 뒤에만
인식된 legacy RTRT plugin을 제거하며, 이전 단계가 실패하면 legacy runtime을 보존합니다.
Setup은 `rtrt-agent`의 정확히 일치하는 bare, pinned, ranged, tuple, object 형식만 소유하고
정확한 `"rtrt-agent@0.1.2"` string 하나로 정규화합니다. 이전의 미게시 `rtrt`와 초안
`rtrt-opencode` 패키지 spec은 외부 값이며 순서를 유지합니다. 위 직접 등록과
`opencode plugin rtrt-agent@0.1.2 --global`은 계속 유효합니다.

관리 agent 소유권 상태는 다음 순서의 첫 비어 있지 않은 root에서 읽습니다.
`$OPENCODE_CONFIG_DIR`, `$XDG_CONFIG_HOME/opencode`, `~/.config/opencode`. 공존은 OMO
4.19.4를 대상으로 CI에서 검사하고 OpenCode 1.18.29에서 직접 검증했으며, 어느 쪽도
미래 버전 지원을 보장하지 않습니다. 통합 RTRT 릴리스 워크플로는 정확한
`rtrt-agent@0.1.2` 패키지를 일치하는 Rust 릴리스와 함께 게시합니다.

## 라이선스

MIT
