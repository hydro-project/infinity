// Components
export { MessageList } from "./components/MessageList";
export { MessageItem } from "./components/MessageItem";
export { SessionSidebar } from "./components/SessionSidebar";
export { InputBar } from "./components/InputBar";
export type { InputBarHandle } from "./components/InputBar";
export { Spinner } from "./components/Spinner";
export { ChoicePicker } from "./components/ChoicePicker";
export type { ChoicePickerHandle } from "./components/ChoicePicker";
export { DiffView } from "./components/DiffView";
export { MigratePicker } from "./components/MigratePicker";

// Types
export type {
  SessionStatus,
  SubthreadInfo,
  SessionInfo,
  ModelInfo,
  RemoteInfo,
  TokenUsage,
  DisplaySegment,
  DaemonMessage,
  ClientMessage,
  ConnectionStatus,
  SpinnerState,
  MessageItem as MessageItemType,
} from "./types";

// Protocol helpers
export {
  parseDaemonMessage,
  serializeClientMessage,
  msgTag,
  msgPayload,
} from "./protocol";
