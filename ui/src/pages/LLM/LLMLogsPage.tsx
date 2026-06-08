import styled from "@emotion/styled";
import {
  Alert,
  Button,
  Descriptions,
  InputNumber,
  Popover,
  Space,
  Switch,
  Table,
  Tag,
  Typography,
} from "antd";
import type { ColumnsType } from "antd/es/table";
import {
  ArrowDownLeft,
  ArrowUpRight,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  RefreshCw,
} from "lucide-react";
import type { Key, ReactNode } from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  getLog,
  type LogEntry,
  searchLogs,
  tailLogs,
} from "../../api/logs";

const Container = styled.div`
  display: flex;
  flex-direction: column;
  gap: var(--spacing-md);
  min-height: 0;
`;

const Header = styled.div`
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--spacing-md);
  flex-wrap: wrap;
`;

const PageTitle = styled.h1`
  margin: 0 0 4px;
  font-size: 24px;
  font-weight: 600;
`;

const Description = styled.p`
  margin: 0;
  color: var(--color-text-secondary);
  font-size: 14px;
`;

const Toolbar = styled.div`
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--spacing-md);
  flex-wrap: wrap;
`;

const DetailGrid = styled.div`
  display: grid;
  grid-template-columns: minmax(280px, 1fr) minmax(280px, 1fr);
  gap: var(--spacing-md);

  @media (max-width: 900px) {
    grid-template-columns: 1fr;
  }
`;

const JsonBlock = styled.pre`
  margin: 8px 0 0;
  padding: 12px;
  max-height: 320px;
  overflow: auto;
  border: 1px solid var(--color-border-base);
  border-radius: 6px;
  background: var(--color-bg-layout);
  color: var(--color-text-base);
  font-size: 12px;
  line-height: 1.45;
  white-space: pre-wrap;
`;

const Mono = styled.span`
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", monospace;
  font-size: 12px;
`;

const TokenPair = styled.div`
  display: inline-flex;
  align-items: center;
  justify-content: flex-end;
  gap: 10px;
  width: 100%;
`;

const TokenValue = styled.span`
  display: inline-flex;
  align-items: center;
  gap: 4px;
  white-space: nowrap;

  svg {
    color: var(--color-text-secondary);
  }
`;

const TimestampCell = styled.div`
  display: flex;
  flex-direction: column;
  gap: 2px;
  line-height: 1.25;
`;

const RelativeTime = styled.span`
  color: var(--color-text-secondary);
  font-size: 12px;
`;

const MessagePreview = styled.span`
  display: inline-block;
  max-width: 360px;
  overflow: hidden;
  color: var(--color-text-secondary);
  text-overflow: ellipsis;
  white-space: nowrap;
  vertical-align: bottom;
`;

const ExpandButton = styled.button`
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  padding: 0;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text-secondary);
  cursor: pointer;

  &:hover {
    background: var(--color-bg-hover);
    color: var(--color-text-base);
  }
`;

const PageControls = styled(Space)`
  .ant-input-number {
    width: 76px;
  }
`;

const EMPTY = "-";
const MESSAGE_PREVIEW_CHARS = 120;

function formatDate(value?: string | null): string {
  if (!value) {
    return EMPTY;
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value;
  }
  const month = date.toLocaleString(undefined, { month: "short" });
  const day = String(date.getDate()).padStart(2, "0");
  const hours = String(date.getHours()).padStart(2, "0");
  const minutes = String(date.getMinutes()).padStart(2, "0");
  const seconds = String(date.getSeconds()).padStart(2, "0");
  return `${month} ${day} ${hours}:${minutes}:${seconds}`;
}

function formatNumber(value?: number | null): string {
  return typeof value === "number" ? value.toLocaleString() : EMPTY;
}

function formatRelativeTime(value?: string | null): string {
  if (!value) {
    return EMPTY;
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return EMPTY;
  }

  const diffSeconds = Math.round((date.getTime() - Date.now()) / 1000);
  const absSeconds = Math.abs(diffSeconds);
  const units: Array<[Intl.RelativeTimeFormatUnit, number]> = [
    ["year", 60 * 60 * 24 * 365],
    ["month", 60 * 60 * 24 * 30],
    ["day", 60 * 60 * 24],
    ["hour", 60 * 60],
    ["minute", 60],
    ["second", 1],
  ];
  const [unit, secondsPerUnit] =
    units.find(([, seconds]) => absSeconds >= seconds) ?? units[units.length - 1];
  const valueInUnit = Math.round(diffSeconds / secondsPerUnit);

  return new Intl.RelativeTimeFormat(undefined, { numeric: "auto" }).format(
    valueInUnit,
    unit,
  );
}

function statusTag(status?: number | null, error?: string | null) {
  if (error) {
    return <Tag color="red">Error</Tag>;
  }
  if (typeof status !== "number") {
    return <Tag>Unknown</Tag>;
  }
  if (status >= 500) {
    return <Tag color="red">{status}</Tag>;
  }
  if (status >= 400) {
    return <Tag color="orange">{status}</Tag>;
  }
  if (status >= 300) {
    return <Tag color="blue">{status}</Tag>;
  }
  return <Tag color="green">{status}</Tag>;
}

function stringify(value: unknown): string {
  if (value === undefined || value === null) {
    return EMPTY;
  }
  return JSON.stringify(value, null, 2);
}

function extractText(value: unknown): string | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) {
    return value.map(extractText).filter(Boolean).join(" ");
  }
  if (typeof value === "object") {
    const record = value as Record<string, unknown>;
    if (record.content !== undefined) {
      return extractText(record.content);
    }
    if (record.text !== undefined) {
      return extractText(record.text);
    }
    if (record.prompt !== undefined) {
      return extractText(record.prompt);
    }
    if (record.messages !== undefined) {
      return extractText(record.messages);
    }
    if (record.input !== undefined) {
      return extractText(record.input);
    }
  }
  return JSON.stringify(value);
}

function promptPreview(log: LogEntry): string {
  if (!log.hasPayload) {
    return EMPTY;
  }
  const text = extractText(log.payload?.requestPrompt)?.replace(/\s+/g, " ").trim();
  if (!text) {
    return "Prompt stored";
  }
  if (text.length <= MESSAGE_PREVIEW_CHARS) {
    return text;
  }
  return `${text.slice(0, MESSAGE_PREVIEW_CHARS).trimEnd()}...`;
}

function TokenPopover({ log, children }: { log: LogEntry; children: ReactNode }) {
  return (
    <Popover
      content={
        <Space direction="vertical" size={2}>
          <Typography.Text>
            Input tokens: {formatNumber(log.usage.inputTokens)}
          </Typography.Text>
          <Typography.Text>
            Output tokens: {formatNumber(log.usage.outputTokens)}
          </Typography.Text>
          <Typography.Text>
            Total: {formatNumber(log.usage.totalTokens)}
          </Typography.Text>
        </Space>
      }
      title="Token usage"
    >
      {children}
    </Popover>
  );
}

function LogDetails({ log }: { log: LogEntry }) {
  return (
    <DetailGrid>
      <div>
        <Descriptions size="small" column={1} bordered>
          <Descriptions.Item label="ID">
            <Mono>{log.id}</Mono>
          </Descriptions.Item>
          <Descriptions.Item label="Trace ID">
            <Mono>{log.traceId || EMPTY}</Mono>
          </Descriptions.Item>
          <Descriptions.Item label="Span ID">
            <Mono>{log.spanId || EMPTY}</Mono>
          </Descriptions.Item>
          <Descriptions.Item label="Started">
            {formatDate(log.startedAt)}
          </Descriptions.Item>
          <Descriptions.Item label="Completed">
            {formatDate(log.completedAt)}
          </Descriptions.Item>
          <Descriptions.Item label="Duration">
            {formatNumber(log.durationMs)} ms
          </Descriptions.Item>
          <Descriptions.Item label="Operation">
            {log.genAi.operationName || EMPTY}
          </Descriptions.Item>
          <Descriptions.Item label="Error">
            {log.error || EMPTY}
          </Descriptions.Item>
        </Descriptions>
        {log.attributes !== undefined && (
          <>
            <Typography.Text strong>Attributes</Typography.Text>
            <JsonBlock>{stringify(log.attributes)}</JsonBlock>
          </>
        )}
      </div>
      <div>
        <Descriptions size="small" column={1} bordered>
          <Descriptions.Item label="Provider">
            {log.genAi.providerName || EMPTY}
          </Descriptions.Item>
          <Descriptions.Item label="Request model">
            {log.genAi.requestModel || EMPTY}
          </Descriptions.Item>
          <Descriptions.Item label="Response model">
            {log.genAi.responseModel || EMPTY}
          </Descriptions.Item>
          <Descriptions.Item label="Input tokens">
            {formatNumber(log.usage.inputTokens)}
          </Descriptions.Item>
          <Descriptions.Item label="Output tokens">
            {formatNumber(log.usage.outputTokens)}
          </Descriptions.Item>
          <Descriptions.Item label="Total tokens">
            {formatNumber(log.usage.totalTokens)}
          </Descriptions.Item>
        </Descriptions>
        {log.payload && (
          <>
            <Typography.Text strong>Payload</Typography.Text>
            <JsonBlock>{stringify(log.payload)}</JsonBlock>
          </>
        )}
      </div>
    </DetailGrid>
  );
}

export const LLMLogsPage = () => {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [expandedLogs, setExpandedLogs] = useState<Record<string, LogEntry>>({});
  const [expandedRowKeys, setExpandedRowKeys] = useState<Key[]>([]);
  const [loading, setLoading] = useState(false);
  const [detailLoadingId, setDetailLoadingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [streamError, setStreamError] = useState<string | null>(null);
  const [streaming, setStreaming] = useState(false);
  const [pageSize, setPageSize] = useState(50);
  const [page, setPage] = useState(1);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [pageCursors, setPageCursors] = useState<(string | null)[]>([null]);
  const seenIds = useRef<Set<string>>(new Set());

  const loadPage = useCallback(
    async (targetPage: number, cursor: string | null) => {
      setLoading(true);
      setError(null);
      try {
        const response = await searchLogs({
          limit: pageSize,
          cursor,
          includeAttributes: false,
          includePayload: true,
          filters: {},
        });
        seenIds.current = new Set(response.logs.map((log) => log.id));
        setLogs(response.logs);
        setPage(targetPage);
        setNextCursor(response.nextCursor ?? null);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setLoading(false);
      }
    },
    [pageSize],
  );

  const resetToFirstPage = useCallback(() => {
    setPageCursors([null]);
    setExpandedRowKeys([]);
    void loadPage(1, null);
  }, [loadPage]);

  useEffect(() => {
    resetToFirstPage();
  }, [resetToFirstPage]);

  useEffect(() => {
    if (!streaming) {
      return;
    }

    const controller = new AbortController();
    setStreamError(null);

    void tailLogs(
      {
        limit: Math.min(pageSize, 100),
        includeAttributes: false,
        includePayload: true,
        filters: {},
      },
      {
        onLog: ({ entry }) => {
          if (page !== 1 || seenIds.current.has(entry.id)) {
            return;
          }
          seenIds.current.add(entry.id);
          setLogs((current) => [entry, ...current].slice(0, pageSize));
        },
        onError: (message) => setStreamError(message),
      },
      controller.signal,
    ).catch((err) => {
      if (controller.signal.aborted) {
        return;
      }
      setStreamError(err instanceof Error ? err.message : String(err));
    });

    return () => controller.abort();
  }, [page, pageSize, streaming]);

  const columns: ColumnsType<LogEntry> = useMemo(
    () => [
      {
        title: "Completed",
        dataIndex: "completedAt",
        key: "completedAt",
        render: (value: string) => (
          <TimestampCell>
            <span>{formatDate(value)}</span>
            <RelativeTime>{formatRelativeTime(value)}</RelativeTime>
          </TimestampCell>
        ),
        width: 210,
      },
      {
        title: "Status",
        key: "status",
        render: (_, record) => statusTag(record.httpStatus, record.error),
        width: 110,
      },
      {
        title: "Provider",
        key: "provider",
        render: (_, record) => record.genAi.providerName || EMPTY,
        width: 150,
      },
      {
        title: "Model",
        key: "model",
        render: (_, record) => record.genAi.responseModel || EMPTY,
      },
      {
        title: "Message",
        key: "message",
        render: (_, record) => (
          <MessagePreview title={promptPreview(record)}>
            {promptPreview(record)}
          </MessagePreview>
        ),
      },
      {
        title: "Tokens",
        key: "tokens",
        align: "right",
        render: (_, record) => (
          <TokenPopover log={record}>
            <TokenPair>
              <TokenValue>
                <ArrowDownLeft size={14} />
                {formatNumber(record.usage.inputTokens)}
              </TokenValue>
              <TokenValue>
                <ArrowUpRight size={14} />
                {formatNumber(record.usage.outputTokens)}
              </TokenValue>
            </TokenPair>
          </TokenPopover>
        ),
        width: 170,
      },
      {
        title: "Payload",
        dataIndex: "hasPayload",
        key: "hasPayload",
        render: (hasPayload: boolean) =>
          hasPayload ? <Tag color="blue">Stored</Tag> : <Tag>None</Tag>,
        width: 110,
      },
      {
        title: "Duration",
        dataIndex: "durationMs",
        key: "durationMs",
        align: "right",
        render: (value: number) => `${formatNumber(value)} ms`,
        width: 120,
      },
    ],
    [],
  );

  const handleExpand = async (expanded: boolean, record: LogEntry) => {
    if (!expanded || expandedLogs[record.id]) {
      return;
    }

    setDetailLoadingId(record.id);
    setError(null);
    try {
      const response = await getLog({ id: record.id, includePayload: true });
      if (response.log) {
        setExpandedLogs((current) => ({ ...current, [record.id]: response.log as LogEntry }));
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setDetailLoadingId(null);
    }
  };

  const goNext = () => {
    if (!nextCursor) {
      return;
    }
    const nextPage = page + 1;
    setPageCursors((current) => {
      const updated = current.slice(0, nextPage - 1);
      updated[nextPage - 1] = nextCursor;
      return updated;
    });
    setExpandedRowKeys([]);
    void loadPage(nextPage, nextCursor);
  };

  const goPrevious = () => {
    if (page <= 1) {
      return;
    }
    const previousPage = page - 1;
    setExpandedRowKeys([]);
    void loadPage(previousPage, pageCursors[previousPage - 1] ?? null);
  };

  const handleStreamingChange = (checked: boolean) => {
    setStreaming(checked);
    if (checked && page !== 1) {
      resetToFirstPage();
    }
  };

  return (
    <Container>
      <Header>
        <div>
          <PageTitle>LLM Logs</PageTitle>
          <Description>
            Completed LLM requests from the request log database.
          </Description>
        </div>
        <Space>
          <Switch checked={streaming} onChange={handleStreamingChange} />
          <Typography.Text>Stream</Typography.Text>
          <Button icon={<RefreshCw size={16} />} onClick={resetToFirstPage}>
            Refresh
          </Button>
        </Space>
      </Header>

      {error && (
        <Alert
          message="Failed to load logs"
          description={error}
          type="error"
          showIcon
        />
      )}
      {streamError && (
        <Alert
          message="Log stream stopped"
          description={streamError}
          type="warning"
          showIcon
        />
      )}

      <Toolbar>
        <PageControls>
          <Button
            icon={<ChevronLeft size={16} />}
            disabled={page <= 1 || loading}
            onClick={goPrevious}
          >
            Previous
          </Button>
          <Typography.Text>Page {page}</Typography.Text>
          <Button
            icon={<ChevronRight size={16} />}
            disabled={!nextCursor || loading}
            onClick={goNext}
          >
            Next
          </Button>
        </PageControls>
        <Space>
          <Typography.Text>Rows</Typography.Text>
          <InputNumber
            min={10}
            max={500}
            step={10}
            value={pageSize}
            onChange={(value) => setPageSize(value ?? 50)}
          />
        </Space>
      </Toolbar>

      <Table
        rowKey="id"
        columns={columns}
        dataSource={logs}
        loading={loading}
        pagination={false}
        scroll={{ x: 1080 }}
        expandable={{
          expandedRowKeys,
          onExpandedRowsChange: (keys) => setExpandedRowKeys([...keys]),
          onExpand: handleExpand,
          expandIcon: ({ expanded, onExpand, record }) => (
            <ExpandButton
              aria-label={expanded ? "Collapse log details" : "Expand log details"}
              onClick={(event) => onExpand(record, event)}
              type="button"
            >
              {expanded ? <ChevronDown size={16} /> : <ChevronRight size={16} />}
            </ExpandButton>
          ),
          expandedRowRender: (record) =>
            detailLoadingId === record.id ? (
              <Typography.Text>Loading details...</Typography.Text>
            ) : (
              <LogDetails log={expandedLogs[record.id] ?? record} />
            ),
        }}
      />
    </Container>
  );
};
