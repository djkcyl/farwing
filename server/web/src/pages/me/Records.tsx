// The account's connection records and events.
import { useState } from 'react';
import { useT } from '../../i18n/react';
import { Card, PageHeader, Tabs } from '../../ui/kit';
import { EventList, RecordList } from '../../ui/Records';

export default function Records() {
  const t = useT();
  const [tab, setTab] = useState<'connections' | 'events'>('connections');
  return (
    <div className="stack">
      <PageHeader title={t.nav.records} />
      <Tabs<'connections' | 'events'>
        value={tab}
        onChange={setTab}
        items={[
          { id: 'connections', label: t.records.connections },
          { id: 'events', label: t.records.events },
        ]}
      />
      <Card pad={false}>{tab === 'connections' ? <RecordList path="/api/v2/records" /> : <EventList />}</Card>
    </div>
  );
}
