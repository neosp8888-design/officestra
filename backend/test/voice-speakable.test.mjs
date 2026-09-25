// 스트리밍 문장 추출기가 Markdown을 읽을 글로 바꾸고 완성된 문장만 내보내는지 확인한다.
import assert from 'node:assert/strict';
import test from 'node:test';
import { DIRECTION_PAUSE_MS, PARAGRAPH_BREAK, createSentenceStream, parseDirections, speakableText, splitSentences } from '../src/voice/speakable.mjs';

test('Markdown 기호·링크·경로를 읽을 글로 바꾼다', () => {
  assert.equal(speakableText('## **결론** 먼저 말씀드릴게요'), '결론 먼저 말씀드릴게요');
  assert.equal(speakableText('- [문서](https://a.b/c) 확인'), '문서 확인');
  assert.equal(speakableText('`Sources/OfficeGame/StreamingPlainTextView.swift` 수정'), 'StreamingPlainTextView.swift 수정');
  assert.equal(speakableText('참고 https://example.com/x?y=1 입니다'), '참고 링크 입니다');
});

test('짧은 조각은 다음 문장에 붙인다', () => {
  assert.deepEqual(splitSentences('네. 확인했습니다. 바로 진행할게요!'), ['네. 확인했습니다.', '바로 진행할게요!']);
});

test('줄이 끝나지 않아도 문장 부호 뒤까지는 바로 내보낸다', () => {
  const stream = createSentenceStream();
  assert.deepEqual(stream.push('첫 문장입니다. 두 번'), ['첫 문장입니다.']);
  assert.deepEqual(stream.push('첫 문장입니다. 두 번째 문장도 끝났어요. 세'), ['두 번째 문장도 끝났어요.']);
  assert.deepEqual(stream.finish(), ['세']);
});

test('코드 블록과 표는 읽지 않는다', () => {
  const stream = createSentenceStream();
  const text = '설명입니다.\n```js\nconst a = 1. b\n```\n| 항목 | 값 |\n| --- | --- |\n---\n끝입니다.\n';
  const spoken = [...stream.push(text.slice(0, 20)), ...stream.push(text), ...stream.finish()];
  assert.deepEqual(spoken, ['설명입니다.', '끝입니다.']);
});

test('한 글자씩 들어와도 같은 문장을 한 번만 낸다', () => {
  const stream = createSentenceStream();
  const text = '## 보고\n작업을 마쳤습니다. 테스트도 통과했어요!\n- 수정 파일은 `a/b/c.swift` 입니다.\n';
  const spoken = [];
  for (let i = 1; i <= text.length; i += 1) spoken.push(...stream.push(text.slice(0, i)));
  spoken.push(...stream.finish());
  assert.deepEqual(spoken, ['보고', '작업을 마쳤습니다.', '테스트도 통과했어요!', '수정 파일은 c.swift 입니다.']);
});

test('괄호 지침은 읽지 않고 말투와 쉼으로 바꾼다', () => {
  assert.deepEqual(parseDirections('(잠시 침묵하다가) 몸으로 나누는 대화라...'),
    { text: '몸으로 나누는 대화라...', style: null, pauseMs: DIRECTION_PAUSE_MS });
  assert.deepEqual(parseDirections('(귓가에 속삭이며) 이건 비밀이야.'), { text: '이건 비밀이야.', style: 'whisper', pauseMs: 0 });
  assert.deepEqual(parseDirections('아, 정말요? (장난스럽게 넘기기)'), { text: '아, 정말요?', style: 'playful', pauseMs: 0 });
  assert.deepEqual(parseDirections('수정 파일（Swift）입니다.'), { text: '수정 파일 입니다.', style: null, pauseMs: 0 });
  // 괄호 안에서 문장이 끊긴 경우
  assert.deepEqual(parseDirections('좋아요 (웃으며'), { text: '좋아요', style: 'playful', pauseMs: 0 });
  assert.deepEqual(parseDirections('한숨) 그래도 해 볼게요.'), { text: '그래도 해 볼게요.', style: null, pauseMs: DIRECTION_PAUSE_MS });
  assert.deepEqual(parseDirections('(웃음)'), { text: '', style: 'playful', pauseMs: 0 });
});

test('문단 표시를 켜면 빈 줄 자리에 문단 끝을 끼워 내보낸다', () => {
  const text = '(울먹이며) 힘든 하루였어요. 그래도 고마워요.\n둘째 줄이에요.\n\n새 문단이에요.\n```\n\n```\n끝.\n';
  const withBreaks = createSentenceStream({ paragraphBreaks: true });
  assert.deepEqual([...withBreaks.push(text), ...withBreaks.finish()],
    ['(울먹이며) 힘든 하루였어요.', '그래도 고마워요.', '둘째 줄이에요.', PARAGRAPH_BREAK, '새 문단이에요.', '끝.']);
  const plain = createSentenceStream();
  assert.ok(![...plain.push(text), ...plain.finish()].includes(PARAGRAPH_BREAK));
});

test('괄호 지침의 감정 낱말로 감정 기준 음성을 고른다', () => {
  const styleOf = sentence => parseDirections(sentence).style;
  assert.equal(styleOf('(울먹이며) 미안해요.'), 'sad');
  assert.equal(styleOf('(배를 움켜쥐고 아파하며) 괜찮아요.'), 'pain');
  assert.equal(styleOf('(화가 나서) 그만하세요.'), 'angry');
  assert.equal(styleOf('(들뜬 목소리로) 정말요?'), 'excited');
  assert.equal(styleOf('(살짝 질투하며) 그 사람이 좋아요?'), 'jealous');
  // 감정과 속삭임이 섞이면 감정을 먼저 본다.
  assert.equal(styleOf('(울먹이며 작게 속삭이듯) 보고 싶었어.'), 'sad');
  // 한숨은 쉼으로, 감정 낱말이 없으면 말투는 그대로 둔다.
  assert.deepEqual(parseDirections('(한숨을 쉬며) 알겠어요.'), { text: '알겠어요.', style: null, pauseMs: DIRECTION_PAUSE_MS });
});
