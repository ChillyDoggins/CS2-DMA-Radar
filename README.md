# CS2 DMA Radar

A CS2 DMA (Direct Memory Access) utility for radar functionality that reads memory from game DLLs to access player positions, weapons, and entity data.

## Building

To build the project:

```bash
build release -r
```

## Features

- Real-time player position visualization
- Maps displayed on web server localhost:8080
- Rotate on View

## Usage

After building with `build release -r`, use the tool to inject into your CS2 session for radar functionality.

---

*Note: For research purposes only. I do not support cheating.*

## Licenses & Dependencies

This project includes compiled binaries and assets from third-party open-source projects:

- **MemProcFS** by [@ufrisk](https://github.com/ufrisk) - Licensed under AGPL-3.0
- **LeechCore** by [@ufrisk](https://github.com/ufrisk) - Licensed under GPL-3.0
