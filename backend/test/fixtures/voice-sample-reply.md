## 확인 결과

요청하신 CPU 측정을 마쳤습니다. 원인은 스트리밍 중인 답변의 Markdown 폭이 매번 무한대로 바뀌는 데 있었어요.

- 과거 답변과 반대쪽 창에는 영향이 없습니다.
- 수정 대상은 `Sources/OfficeGame/SelectableMarkdownTextView.swift` 한 곳입니다.

```swift
guard let width = proposal.width, width.isFinite else { return nil }
```

| 항목 | 값 |
| --- | --- |
| 개선 | 확인 필요 |

이 정도면 바로 고칠 수 있을 것 같아요! 진행해도 될까요?
