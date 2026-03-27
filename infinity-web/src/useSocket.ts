import { useEffect, useRef, useCallback, useState } from 'react';
import type { ClientMessage, DaemonMessage } from './types';
import { parseDaemonMessage, serializeClientMessage } from './protocol';

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected';

interface UseSocketOptions {
  url: string;
  onMessage: (msg: DaemonMessage) => void;
}

export function useSocket({ url, onMessage }: UseSocketOptions) {
  const wsRef = useRef<WebSocket | null>(null);
  const onMessageRef = useRef(onMessage);
  onMessageRef.current = onMessage;
  const [status, setStatus] = useState<ConnectionStatus>('connecting');

  useEffect(() => {
    let cancelled = false;
    let reconnectTimer: ReturnType<typeof setTimeout>;

    function connect() {
      if (cancelled) return;
      setStatus('connecting');
      const ws = new WebSocket(url);
      wsRef.current = ws;

      ws.onopen = () => {
        if (cancelled) { ws.close(); return; }
        setStatus('connected');
      };

      ws.onmessage = (ev) => {
        try {
          const msg = parseDaemonMessage(ev.data as string);
          onMessageRef.current(msg);
        } catch {
          // ignore malformed messages
        }
      };

      ws.onclose = () => {
        if (cancelled) return;
        setStatus('disconnected');
        reconnectTimer = setTimeout(connect, 2000);
      };

      ws.onerror = () => ws.close();
    }

    connect();

    return () => {
      cancelled = true;
      clearTimeout(reconnectTimer);
      wsRef.current?.close();
    };
  }, [url]);

  const send = useCallback((msg: ClientMessage) => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(serializeClientMessage(msg));
    }
  }, []);

  return { send, status };
}
