import type { KeyboardEvent, RefObject } from 'react';

type ComposerProps = {
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  onStop: () => void;
  loading: boolean;
  inputRef: RefObject<HTMLTextAreaElement | null>;
};

export function Composer({ value, onChange, onSend, onStop, loading, inputRef }: ComposerProps) {
  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== 'Enter' || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    onSend();
  };

  return (
    <form
      className="composer"
      onSubmit={(event) => {
        event.preventDefault();
        onSend();
      }}
    >
      <label className="sr-only" htmlFor="composer-input">
        Message
      </label>
      <textarea
        id="composer-input"
        ref={inputRef}
        value={value}
        rows={3}
        placeholder="Describe the task"
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={handleKeyDown}
      />
      <div className="composer-actions">
        <span className="composer-hint">Enter to send · Shift+Enter for a newline</span>
        {loading ? (
          <button type="button" className="stop" onClick={onStop}>
            Stop
          </button>
        ) : (
          <button type="submit" disabled={value.trim() === ''}>
            Send
          </button>
        )}
      </div>
    </form>
  );
}
