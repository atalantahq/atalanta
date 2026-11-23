import { useEffect } from "react";
import { currentDevice, listDevices, selectDevice } from "../api/client";
import { useInspectorStore } from "../store/inspectorStore";

export function DevicePicker() {
  const { devices, currentDeviceId, setDevices, setCurrentDeviceId, setError } =
    useInspectorStore();

  useEffect(() => {
    listDevices()
      .then(setDevices)
      .catch((e) => setError(e.message));
    // Start on the device the server picked (`atalanta inspect -d`, or the
    // booted one it found).
    currentDevice()
      .then((id) => {
        if (id && !useInspectorStore.getState().currentDeviceId) setCurrentDeviceId(id);
      })
      .catch(() => {});
  }, [setDevices, setError, setCurrentDeviceId]);

  const handleChange = async (e: React.ChangeEvent<HTMLSelectElement>) => {
    const id = e.target.value;
    if (!id) return;
    try {
      await selectDevice(id);
      setCurrentDeviceId(id);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Failed to select device");
    }
  };

  const booted = devices.filter((d) => d.state === "booted");

  return (
    <div className="device-picker">
      <label>Device: </label>
      <select value={currentDeviceId ?? ""} onChange={handleChange}>
        <option value="">Select a device...</option>
        {booted.map((d) => (
          <option key={d.id} value={d.id}>
            {d.name} ({d.platform}{d.os_version ? `, ${d.os_version}` : ""})
          </option>
        ))}
      </select>
      {booted.length === 0 && devices.length > 0 && (
        <span className="hint">No booted devices found</span>
      )}
    </div>
  );
}
