// Every connection record on this server.
import { useT } from '../../i18n/react';
import { Card, PageHeader } from '../../ui/kit';
import { RecordList } from '../../ui/Records';

export default function Records() {
  const t = useT();
  return (
    <div className="stack">
      <PageHeader title={t.nav.allRecords} />
      <Card pad={false}>
        <RecordList path="/api/v2/admin/records" />
      </Card>
    </div>
  );
}
