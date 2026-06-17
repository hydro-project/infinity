import {
  useRef,
  useLayoutEffect,
  useCallback,
  forwardRef,
  useImperativeHandle,
} from "react";
import type { MessageItem as MsgItem, SpinnerState } from "../types";
import { MessageList, type MessageListHandle } from "./MessageList";
import { InputBar, type InputBarHandle } from "./InputBar";
import { ChoicePicker } from "./ChoicePicker";
import css from "./ChatView.module.css";

interface PendingChoice {
  prompt: string;
  choices: string[];
  default: number;
}

interface Props {
  messages: MsgItem[];
  generation: number;
  spinner: SpinnerState | null;
  onSend: (text: string) => void;
  inputDisabled: boolean;
  pendingChoice: PendingChoice | null;
  onChoiceSelect: (index: number) => void;
  theme?: "light" | "dark";
  embeddedInput?: string;
  initialInputValue?: string;
  onInputValueChange?: (value: string) => void;
}

export interface ChatViewHandle {
  focus: () => void;
}

export const ChatView = forwardRef<ChatViewHandle, Props>(function ChatView(
  {
    messages,
    generation,
    spinner,
    onSend,
    inputDisabled,
    pendingChoice,
    onChoiceSelect,
    theme,
    embeddedInput,
    initialInputValue,
    onInputValueChange,
  },
  fwdRef,
) {
  const messageListRef = useRef<MessageListHandle>(null);
  const inputBarRef = useRef<InputBarHandle>(null);

  useImperativeHandle(
    fwdRef,
    () => ({ focus: () => inputBarRef.current?.focus() }),
    [],
  );

  const handleSend = useCallback(
    (text: string) => {
      onSend(text);
      messageListRef.current?.scrollToBottom();
    },
    [onSend],
  );

  useLayoutEffect(() => {
    if (pendingChoice) {
      messageListRef.current?.scrollToBottom();
    }
  }, [pendingChoice]);

  return (
    <div className={css.container}>
      <MessageList
        ref={messageListRef}
        messages={messages}
        generation={generation}
        theme={theme}
      />
      <div className={css.inputArea}>
        {pendingChoice && (
          <ChoicePicker
            prompt={pendingChoice.prompt}
            choices={pendingChoice.choices}
            defaultIndex={pendingChoice.default}
            onSelect={onChoiceSelect}
            onFocusInput={() => inputBarRef.current?.focus()}
          />
        )}
        <InputBar
          ref={inputBarRef}
          onSend={handleSend}
          disabled={inputDisabled}
          spinner={spinner}
          embeddedInput={embeddedInput}
          initialValue={initialInputValue}
          onValueChange={onInputValueChange}
        />
      </div>
    </div>
  );
});
