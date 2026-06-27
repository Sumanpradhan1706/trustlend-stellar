"use client";

interface ExportCsvButtonProps {
  data: any[];
  filename?: string;
}

export function ExportCsvButton({ data, filename = "transactions.csv" }: ExportCsvButtonProps) {
  const handleExport = () => {
    if (!data || data.length === 0) return;
    
    // Extract headers from the first object
    const headers = Object.keys(data[0]);
    
    // Build CSV string
    const csvRows = [];
    csvRows.push(headers.join(",")); // Header row
    
    for (const row of data) {
      const values = headers.map(header => {
        const val = row[header];
        // Escape quotes and wrap in quotes to handle commas within values
        const escaped = String(val ?? "").replace(/"/g, '""');
        return `"${escaped}"`;
      });
      csvRows.push(values.join(","));
    }
    
    const csvString = csvRows.join("\n");
    const blob = new Blob([csvString], { type: "text/csv;charset=utf-8;" });
    const url = URL.createObjectURL(blob);
    
    const link = document.createElement("a");
    link.href = url;
    link.setAttribute("download", filename);
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  return (
    <button
      onClick={handleExport}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: "0.5rem",
        padding: "0.4rem 0.85rem",
        borderRadius: "0.4rem",
        background: "#111827",
        color: "#fff",
        border: "none",
        fontWeight: 600,
        fontSize: "0.8rem",
        cursor: "pointer",
        transition: "background 0.2s"
      }}
      onMouseOver={(e) => e.currentTarget.style.background = "#374151"}
      onMouseOut={(e) => e.currentTarget.style.background = "#111827"}
    >
      <span>⬇️</span> Export CSV
    </button>
  );
}
