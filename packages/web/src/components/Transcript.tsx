import { useMemo, type ReactNode } from 'react';
import type { UIMessage } from '@tanstack/ai-react';
import { TextPart } from '@tanstack/ai-react/ui';
import { ToolCallCard, ToolResultCard } from './ToolCallCard';

type TranscriptProps = {
  messages: readonly UIMessage[];
};

export function Transcript({ messages }: TranscriptProps) {
  const answeredCalls = useMemo(() => collectAnsweredToolCalls(messages), [messages]);

  return (
    <div className="transcript-inner">
      {messages.map((message, index) => (
        <MessageRow key={`${message.id}-${index}`} message={message} answeredCalls={answeredCalls} />
      ))}
    </div>
  );
}

type MessageRowProps = {
  message: UIMessage;
  answeredCalls: Set<string>;
};

function MessageRow({ message, answeredCalls }: MessageRowProps) {
  switch (message.role) {
    case 'user': {
      const text = textContent(message);
      if (text === '') return null;
      return (
        <div className="message-row message-user">
          <div className="bubble">{text}</div>
        </div>
      );
    }
    case 'assistant': {
      const parts = message.parts.flatMap((part, index) =>
        renderPart(part, `${message.id}-${index}`, answeredCalls),
      );
      if (parts.length === 0) return null;
      return (
        <div className="message-row message-assistant">
          <div className="assistant-body">{parts}</div>
        </div>
      );
    }
    case 'system':
    case 'activity': {
      const text = textContent(message);
      if (text === '') return null;
      return <div className="status-line">{text}</div>;
    }
  }
}

function renderPart(
  part: UIMessage['parts'][number],
  key: string,
  answeredCalls: Set<string>,
): ReactNode[] {
  switch (part.type) {
    case 'text':
      return part.content === '' ? [] : [
        <TextPart
          key={key}
          content={part.content}
          className="text-part"
          components={{
            a: (props) => <a {...props} target="_blank" rel="noreferrer" />,
          }}
        />,
      ];
    case 'thinking':
      return part.content === '' ? [] : [
        <details key={key} className="thinking">
          <summary>Thinking</summary>
          <div className="thinking-body">{part.content}</div>
        </details>,
      ];
    case 'tool-call':
      return [<ToolCallCard key={key} part={part} />];
    case 'tool-result':
      return answeredCalls.has(part.toolCallId) ? [] : [<ToolResultCard key={key} part={part} />];
    case 'image': {
      const src = imageSource(part);
      return src === null ? [] : [<img key={key} className="message-image" src={src} alt="Attachment" />];
    }
    default:
      return [];
  }
}

function textContent(message: UIMessage): string {
  return message.parts.flatMap((part) => (part.type === 'text' ? [part.content] : [])).join('\n');
}

function collectAnsweredToolCalls(messages: readonly UIMessage[]): Set<string> {
  const answered = new Set<string>();
  for (const message of messages) {
    for (const part of message.parts) {
      if (part.type === 'tool-call' && part.output !== undefined) answered.add(part.id);
    }
  }
  return answered;
}

function imageSource(part: Extract<UIMessage['parts'][number], { type: 'image' }>): string | null {
  const source = part.source;
  switch (source.type) {
    case 'data':
      return `data:${source.mimeType};base64,${source.value}`;
    case 'url':
      return source.value;
    case 'file':
      return null;
  }
}
