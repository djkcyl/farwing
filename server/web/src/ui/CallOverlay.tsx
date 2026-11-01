// Over every page while signed in: a call ringing for this account, and
// what ended this browser's voice.
import { useEffect, useState } from 'react';
import { useT } from '../i18n/react';
import { navigate } from '../lib/router';
import { chat, useChat } from '../lib/chat/store';
import { userName } from '../lib/chat/types';
import { useVoice, voice } from '../lib/chat/voice';
import { Button } from './kit';
import { useOverlay } from './overlay';

export function CallOverlay() {
  const t = useT();
  const state = useChat();
  const v = useVoice();
  const { toast } = useOverlay();
  const [dismissed, setDismissed] = useState<string>();
  useEffect(() => {
    if (v.error) toast(v.error, 'red');
  }, [v.error, toast]);
  const me = state.snap?.me.user_id;
  const ringing = state.snap?.calls.find((c) => c.state === 'ringing' && c.callee.user_id === me);
  if (!ringing || dismissed === ringing.id) return null;
  const caller = ringing.caller;
  const avatar = caller.avatar ? chat.media(`/media/avatars/${caller.user_id}?v=${caller.avatar}`) : undefined;
  const name = userName(caller);
  return (
    <div className="call-incoming" role="dialog" aria-label={t.chat.voice.incoming(name)}>
      <span className="chat-avatar" style={{ width: 56, height: 56, fontSize: 24 }}>
        {avatar ? <img src={avatar} alt="" draggable={false} /> : <span>{Array.from(name)[0]?.toUpperCase()}</span>}
      </span>
      <b className="ellipsis">{t.chat.voice.incoming(name)}</b>
      <div className="row gap-sm">
        <Button
          onClick={() => {
            setDismissed(ringing.id);
            voice.decline(ringing.id);
          }}
        >
          {t.chat.voice.decline}
        </Button>
        <Button
          variant="primary"
          icon="phone"
          onClick={() => {
            setDismissed(ringing.id);
            navigate(`/chat/dm/${ringing.channel_id}`);
            void voice.answer(ringing.id, false);
          }}
        >
          {t.chat.voice.answer}
        </Button>
      </div>
    </div>
  );
}
