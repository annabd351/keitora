// Ordered editor for [dispatch].steps: single-line prompt inputs with add,
// remove, and drag-to-reorder (via @dnd-kit/sortable, the same family the Board
// uses). Empty list → Keitora's built-in dispatch prompt. Placeholders are
// interpolated at dispatch time; this editor stores raw text.

import {
  closestCenter,
  DndContext,
  type DragEndEvent,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import {
  arrayMove,
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { GripVertical, Plus, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";

interface Row {
  id: string;
  text: string;
}

const PLACEHOLDERS = "{task_number} {task_id} {task_title} {task_description} {branch} {worktree}";

function arraysEqual(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

function SortableRow({
  row,
  onText,
  onRemove,
}: {
  row: Row;
  onText: (text: string) => void;
  onRemove: () => void;
}) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: row.id,
  });
  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    opacity: isDragging ? 0.6 : 1,
  };
  return (
    <div ref={setNodeRef} style={style} className="flex items-center gap-1.5">
      <button
        type="button"
        className="flex-none cursor-grab text-tx-3 hover:text-tx-1 active:cursor-grabbing"
        title="Drag to reorder"
        {...attributes}
        {...listeners}
      >
        <GripVertical size={14} />
      </button>
      <input
        className="w-full bg-bg-3 border border-line rounded-chip px-2.5 py-1.5 text-tx-1 text-[12.5px] outline-none focus:border-accent/60"
        value={row.text}
        onChange={(e) => onText(e.target.value)}
        placeholder="/clear, /workflow #{task_number}, …"
      />
      <button
        type="button"
        className="flex-none text-tx-3 hover:text-urgent"
        onClick={onRemove}
        title="Remove line"
      >
        <X size={14} />
      </button>
    </div>
  );
}

export function DispatchStepsEditor({
  steps,
  onChange,
}: {
  steps: string[];
  onChange: (steps: string[]) => void;
}) {
  const idCounter = useRef(0);
  const newId = () => `s${idCounter.current++}`;
  const [rows, setRows] = useState<Row[]>(() => steps.map((t) => ({ id: newId(), text: t })));
  // Last value we emitted upward — lets us distinguish a user edit (ignore) from
  // an external re-seed (rebuild rows + regenerate ids).
  const lastEmitted = useRef<string[]>(steps);

  // biome-ignore lint/correctness/useExhaustiveDependencies: newId is a stable inline function created from a ref; intentionally excluded from deps
  useEffect(() => {
    if (!arraysEqual(steps, lastEmitted.current)) {
      setRows(steps.map((t) => ({ id: newId(), text: t })));
      lastEmitted.current = steps;
    }
  }, [steps]);

  const emit = (next: Row[]) => {
    setRows(next);
    const texts = next.map((r) => r.text);
    lastEmitted.current = texts;
    onChange(texts);
  };

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const onDragEnd = (e: DragEndEvent) => {
    const { active, over } = e;
    if (!over || active.id === over.id) return;
    const from = rows.findIndex((r) => r.id === active.id);
    const to = rows.findIndex((r) => r.id === over.id);
    if (from === -1 || to === -1) return;
    emit(arrayMove(rows, from, to));
  };

  return (
    <div className="flex flex-col gap-2">
      {rows.length === 0 ? (
        <p className="text-[11px] text-tx-3">Using Keitora's built-in dispatch prompt.</p>
      ) : (
        <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
          <SortableContext items={rows.map((r) => r.id)} strategy={verticalListSortingStrategy}>
            <div className="flex flex-col gap-1.5">
              {rows.map((row, i) => (
                <SortableRow
                  key={row.id}
                  row={row}
                  onText={(text) => emit(rows.map((r, j) => (j === i ? { ...r, text } : r)))}
                  onRemove={() => emit(rows.filter((_, j) => j !== i))}
                />
              ))}
            </div>
          </SortableContext>
        </DndContext>
      )}

      <div className="flex items-center justify-between">
        <button
          type="button"
          className="flex items-center gap-1 text-[11.5px] text-tx-2 hover:text-tx-1"
          onClick={() => emit([...rows, { id: newId(), text: "" }])}
        >
          <Plus size={13} /> Add line
        </button>
      </div>

      <p className="text-[10.5px] leading-relaxed text-tx-3">
        Each line is typed into the worktree and submitted on dispatch. Placeholders:{" "}
        <span className="font-mono text-tx-2">{PLACEHOLDERS}</span>
      </p>
    </div>
  );
}
