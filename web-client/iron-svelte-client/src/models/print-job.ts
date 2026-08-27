export type PrintJobStatus = 'printing' | 'ready' | 'error';

export interface PrintJobEntry {
    id: number;
    status: PrintJobStatus;
    url?: string;
    error?: string;
}
