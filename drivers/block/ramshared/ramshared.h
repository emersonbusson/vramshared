/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Author: Emerson Busson
 * Email: emersonbusson@gmail.com
 * GitHub: https://github.com/emersonbusson
 * LinkedIn: https://www.linkedin.com/in/emersonbusson
 * Source: https://github.com/emersonbusson/vramshared
 */
#ifndef _RAMSHARED_H
#define _RAMSHARED_H

#include <linux/types.h>
#include <linux/blkdev.h>
#include <linux/blk-mq.h>
#include <linux/pci.h>
#include <linux/mutex.h>
#include <linux/atomic.h>
#include <linux/cache.h>

#define RAMSHARED_DRIVER_NAME		"ramshared"
#define RAMSHARED_DRIVER_VERSION	"1.0.0"
#define RAMSHARED_SECTOR_SIZE		512
#define RAMSHARED_SECTOR_SHIFT		9
#define RAMSHARED_DEFAULT_QUEUE_DEPTH	256
#define RAMSHARED_MMIO_POISON_VALUE	0xFFFFFFFFU

/**
 * struct ramshared_dma_region - PCIe DMA memory descriptor
 * @pci_addr: Physical PCIe bus address
 * @cpu_addr: Kernel virtual address mapped with write-combining
 * @size: Size of the allocated region in bytes
 * @dma_handle: DMA bus address mapping
 */
struct ramshared_dma_region {
	phys_addr_t	pci_addr;
	void __iomem	*cpu_addr;
	size_t		size;
	dma_addr_t	dma_handle;
};

/**
 * enum ramshared_dev_state - backing-store availability
 * @RAMSHARED_DEV_LIVE: aperture is mapped and the contents are trusted
 * @RAMSHARED_DEV_IO_FROZEN: a channel error gated new requests; contents
 *	are unprovable and must not be served as intact
 * @RAMSHARED_DEV_UNAVAILABLE: backing contents are lost or unprovable;
 *	requests fail with a stable error instead of claiming recovery
 *
 * AUD-13: an AER event can invalidate the aperture while the block surface
 * would otherwise resume as healthy. The state gates every request.
 */
enum ramshared_dev_state {
	RAMSHARED_DEV_LIVE,
	RAMSHARED_DEV_IO_FROZEN,
	RAMSHARED_DEV_UNAVAILABLE,
};

/**
 * struct ramshared_device - Main in-tree block device state
 * @disk: gendisk descriptor
 * @tag_set: blk-mq tag set
 * @dma: DMA memory region
 * @capacity_bytes: Total available VRAM capacity in bytes
 * @dev: Pointer to underlying struct device
 * @lock: Mutex protecting device state transitions
 * @state: Backing-store availability (enum ramshared_dev_state)
 * @bar0_len: BAR0 length observed at successful init, revalidated after reset
 * @dma_transfers_total: Cold diagnostic counter for completed transfers
 * @read_bytes: Hot-path total bytes read (own cache line)
 * @write_bytes: Hot-path total bytes written
 */
struct ramshared_device {
	struct gendisk			*disk;
	struct blk_mq_tag_set		tag_set;
	struct ramshared_dma_region	dma;
	u64				capacity_bytes;
	struct device			*dev;
	/* Mutex serializing device registration and teardown */
	struct mutex			lock;
	enum ramshared_dev_state	state;
	resource_size_t			bar0_len;
	/* Cold / diagnostic counters (separate cache line — KRYPTON-07) */
	atomic64_t			dma_transfers_total ____cacheline_aligned_in_smp;
	/* Hot I/O path counters (isolated cache line — KRYPTON-07) */
	atomic64_t			read_bytes ____cacheline_aligned_in_smp;
	atomic64_t			write_bytes;
};

int ramshared_dma_init(struct ramshared_device *rs_dev, struct pci_dev *pdev);
void ramshared_dma_cleanup(struct ramshared_device *rs_dev);
int ramshared_queue_init(struct ramshared_device *rs_dev,
			 struct device *parent_dev, unsigned int q_depth);
void ramshared_queue_cleanup(struct ramshared_device *rs_dev);

struct ramshared_info {
	u64 capacity_bytes;
	u32 queue_depth;
	u32 reserved;
};

struct ramshared_param {
	u32 param_id;
	u32 value;
};

#define RAMSHARED_IOC_MAGIC 'R'
#define RAMSHARED_IOC_GET_INFO	_IOR(RAMSHARED_IOC_MAGIC, 1, struct ramshared_info)
#define RAMSHARED_IOC_SET_PARAM	_IOW(RAMSHARED_IOC_MAGIC, 2, struct ramshared_param)

#include "compat.h"

extern const struct attribute_group *ramshared_attr_groups[];

int ramshared_ioctl(struct block_device *bdev, ramshared_blk_mode_t mode,
		    unsigned int cmd, unsigned long arg);
#endif /* _RAMSHARED_H */
