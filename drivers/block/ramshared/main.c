// SPDX-License-Identifier: GPL-2.0-only
/*
 * Author: Emerson Busson
 * Email: emersonbusson@gmail.com
 * GitHub: https://github.com/emersonbusson
 * LinkedIn: https://www.linkedin.com/in/emersonbusson
 * Source: https://github.com/emersonbusson/vramshared
 */
/*
 * RamShared - Hardware-Accelerated VRAM Block Driver
 *
 * Copyright (C) 2026 Emerson Busson <emersonbusson@gmail.com>
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/pci.h>
#include <linux/dma-mapping.h>
#include "ramshared.h"

MODULE_AUTHOR("Emerson Busson <emersonbusson@gmail.com>");
MODULE_DESCRIPTION("Hardware-Accelerated VRAM Block Driver");
MODULE_LICENSE("GPL");
MODULE_VERSION(RAMSHARED_DRIVER_VERSION);

static unsigned long capacity_mb = 1024;
module_param(capacity_mb, ulong, 0444);
MODULE_PARM_DESC(capacity_mb, "Initial VRAM block device capacity in MiB (default: 1024)");

static unsigned int queue_depth = RAMSHARED_DEFAULT_QUEUE_DEPTH;
module_param(queue_depth, uint, 0444);
MODULE_PARM_DESC(queue_depth, "Hardware queue depth (default: 256)");

/*
 * AUD-12: PCI class and BAR length establish range, never meaning or
 * ownership. The driver binds only the exact device whose BAR0 the platform
 * integrator has attested as a dedicated storage aperture. Without that
 * attestation the probe refuses: treating an arbitrary display or
 * accelerator BAR0 as storage can corrupt device state or another owner's
 * contents. This is an explicit per-device contract, not a supported-device
 * list; the surface stays unqualified until a real resource contract exists.
 */
static char *storage_aperture;
module_param(storage_aperture, charp, 0444);
MODULE_PARM_DESC(storage_aperture,
		 "PCI slot name (e.g. 0000:01:00.0) whose BAR0 is an attested dedicated storage aperture");

static int ramshared_pci_probe(struct pci_dev *pdev,
			       const struct pci_device_id *id)
{
	struct ramshared_device *rs_dev;
	resource_size_t bar_len;
	u64 capacity_bytes;
	int ret;

	if (!pdev || !id)
		return -EINVAL;

	dev_info(&pdev->dev, "probing RamShared hardware (capacity=%lu MiB)\n",
		 capacity_mb);

	if (!storage_aperture || !storage_aperture[0]) {
		dev_err(&pdev->dev,
			"refusing %s: no attested dedicated storage aperture (AUD-12)\n",
			pci_name(pdev));
		return -ENODEV;
	}

	if (strcmp(pci_name(pdev), storage_aperture) != 0) {
		dev_err(&pdev->dev,
			"refusing %s: not the attested storage aperture '%s'\n",
			pci_name(pdev), storage_aperture);
		return -ENODEV;
	}

	if (capacity_mb == 0 || capacity_mb > (1UL << 20)) {
		dev_err(&pdev->dev, "invalid capacity_mb parameter: %lu\n",
			capacity_mb);
		return -ERANGE;
	}

	if (check_mul_overflow((u64)capacity_mb, 1024ULL * 1024ULL,
			       &capacity_bytes)) {
		dev_err(&pdev->dev, "capacity_bytes integer overflow\n");
		return -EOVERFLOW;
	}

	bar_len = pci_resource_len(pdev, 0);
	/* SPEC: kernel-pci-bar-capacity-contract §RF-1. */
	if (bar_len == 0 || (u64)bar_len < capacity_bytes) {
		dev_err(&pdev->dev,
			"BAR0 is smaller than requested capacity (%llu < %llu)\n",
			(unsigned long long)bar_len,
			(unsigned long long)capacity_bytes);
		return -ERANGE;
	}

	if (queue_depth < 16 || queue_depth > 1024) {
		dev_warn(&pdev->dev,
			 "clamping queue_depth (%u) to bounds [16, 1024]\n",
			 queue_depth);
		if (queue_depth < 16)
			queue_depth = 16;
		else
			queue_depth = 1024;
	}

	rs_dev = devm_kzalloc(&pdev->dev, sizeof(*rs_dev), GFP_KERNEL);
	if (!rs_dev)
		return -ENOMEM;

	rs_dev->dev = &pdev->dev;
	rs_dev->capacity_bytes = capacity_bytes;
	rs_dev->state = RAMSHARED_DEV_LIVE;
	rs_dev->bar0_len = bar_len;
	mutex_init(&rs_dev->lock);
	atomic64_set(&rs_dev->dma_transfers_total, 0);
	atomic64_set(&rs_dev->read_bytes, 0);
	atomic64_set(&rs_dev->write_bytes, 0);

	ret = pci_enable_device_mem(pdev);
	if (ret) {
		dev_err(&pdev->dev, "failed to enable PCIe memory device: %d\n",
			ret);
		return ret;
	}

	pci_set_master(pdev);

	ret = dma_set_mask_and_coherent(&pdev->dev, DMA_BIT_MASK(64));
	if (ret) {
		dev_warn(&pdev->dev, "64-bit DMA failed, attempting 32-bit DMA\n");
		ret = dma_set_mask_and_coherent(&pdev->dev, DMA_BIT_MASK(32));
		if (ret) {
			dev_err(&pdev->dev, "no usable DMA configuration\n");
			goto err_clear_master;
		}
	}

	ret = pci_request_mem_regions(pdev, RAMSHARED_DRIVER_NAME);
	if (ret) {
		dev_err(&pdev->dev, "failed to claim PCIe memory regions\n");
		ret = -EBUSY;
		goto err_clear_master;
	}

	ret = ramshared_dma_init(rs_dev, pdev);
	if (ret)
		goto err_release_regions;

	ret = ramshared_queue_init(rs_dev, &pdev->dev, queue_depth);
	if (ret)
		goto err_dma_cleanup;

	ret = device_add_disk(&pdev->dev, rs_dev->disk, ramshared_attr_groups);
	if (ret) {
		dev_err(&pdev->dev, "failed to add block disk (err=%d)\n", ret);
		put_disk(rs_dev->disk);
		rs_dev->disk = NULL;
		blk_mq_free_tag_set(&rs_dev->tag_set);
		goto err_dma_cleanup;
	}

	pci_set_drvdata(pdev, rs_dev);
	dev_info(&pdev->dev, "block device /dev/%s registered successfully\n",
		 rs_dev->disk->disk_name);
	return 0;

err_dma_cleanup:
	ramshared_dma_cleanup(rs_dev);
err_release_regions:
	pci_release_mem_regions(pdev);
err_clear_master:
	pci_clear_master(pdev);
	pci_disable_device(pdev);
	return ret;
}

static void ramshared_pci_remove(struct pci_dev *pdev)
{
	struct ramshared_device *rs_dev;

	if (!pdev)
		return;

	rs_dev = pci_get_drvdata(pdev);
	if (!rs_dev)
		return;

	ramshared_queue_cleanup(rs_dev);
	ramshared_dma_cleanup(rs_dev);
	pci_release_mem_regions(pdev);
	pci_clear_master(pdev);
	pci_disable_device(pdev);

	dev_info(&pdev->dev, "RamShared device removed successfully\n");
}

/*
 * PCIe AER & Link Drop Error Handlers.
 *
 * AUD-13: a reset may invalidate the aperture or its contents while the
 * block surface would otherwise resume as healthy. New requests are gated
 * before the channel is classified, ownership of the mapping is retained,
 * and the backing store is restored only after explicit resource and origin
 * proof. Irreversible loss is reported as a stable I/O failure, never as a
 * recovery.
 */
static pci_ers_result_t ramshared_pci_error_detected(struct pci_dev *pdev,
						     pci_channel_state_t state)
{
	struct ramshared_device *rs_dev;

	dev_err(&pdev->dev, "PCIe error detected (state=%d)\n", state);

	rs_dev = pci_get_drvdata(pdev);
	if (!rs_dev)
		return PCI_ERS_RESULT_DISCONNECT;

	if (rs_dev->state == RAMSHARED_DEV_LIVE)
		rs_dev->state = RAMSHARED_DEV_IO_FROZEN;

	/*
	 * Gate new requests before the channel is classified. The aperture
	 * contents are already unprovable; the block surface must not keep
	 * serving them as intact.
	 */
	if (rs_dev->disk && rs_dev->disk->queue)
		blk_mq_stop_hw_queues(rs_dev->disk->queue);

	if (state == pci_channel_io_perm_failure) {
		rs_dev->state = RAMSHARED_DEV_UNAVAILABLE;
		return PCI_ERS_RESULT_DISCONNECT;
	}

	if (state == pci_channel_io_frozen)
		return PCI_ERS_RESULT_NEED_RESET;

	return PCI_ERS_RESULT_CAN_RECOVER;
}

static pci_ers_result_t ramshared_pci_slot_reset(struct pci_dev *pdev)
{
	struct ramshared_device *rs_dev;
	resource_size_t bar_start, bar_len;

	dev_info(&pdev->dev, "PCIe slot reset recovery initiated\n");

	rs_dev = pci_get_drvdata(pdev);
	if (!rs_dev)
		return PCI_ERS_RESULT_DISCONNECT;

	pci_restore_state(pdev);

	/*
	 * Resource proof: the aperture must be the same one this device was
	 * bound to. A changed, missing, or shortened BAR is not a recovery.
	 */
	bar_start = pci_resource_start(pdev, 0);
	bar_len = pci_resource_len(pdev, 0);
	if (!bar_start || bar_len != rs_dev->bar0_len ||
	    (u64)bar_len < rs_dev->capacity_bytes) {
		dev_err(&pdev->dev,
			"BAR0 failed revalidation after reset (start=%pa len=%llu expected=%llu cap=%llu)\n",
			&bar_start, (unsigned long long)bar_len,
			(unsigned long long)rs_dev->bar0_len,
			(unsigned long long)rs_dev->capacity_bytes);
		rs_dev->state = RAMSHARED_DEV_UNAVAILABLE;
		return PCI_ERS_RESULT_DISCONNECT;
	}

	/*
	 * Origin proof: this driver holds no durable origin from which to
	 * reconstruct aperture contents. Resource proof alone does not
	 * authorize a restore, so the block surface stays gated and every
	 * request fails with a stable error instead of claiming recovery.
	 */
	dev_err(&pdev->dev,
		"BAR0 revalidated but aperture contents are unprovable; disconnecting block surface\n");
	rs_dev->state = RAMSHARED_DEV_UNAVAILABLE;
	return PCI_ERS_RESULT_DISCONNECT;
}

static void ramshared_pci_resume(struct pci_dev *pdev)
{
	struct ramshared_device *rs_dev;

	dev_info(&pdev->dev, "PCIe link resumed\n");

	rs_dev = pci_get_drvdata(pdev);
	if (!rs_dev)
		return;

	/*
	 * Resume restores the link, not the backing store. The queue stays
	 * stopped and the state gate stays closed; see
	 * ramshared_pci_slot_reset.
	 */
	dev_warn(&pdev->dev,
		 "link resumed with backing-store state %d; requests stay gated\n",
		 rs_dev->state);
}

static const struct pci_error_handlers ramshared_pci_err_handler = {
	.error_detected = ramshared_pci_error_detected,
	.slot_reset     = ramshared_pci_slot_reset,
	.resume         = ramshared_pci_resume,
};

static const struct pci_device_id ramshared_pci_tbl[] = {
	{ PCI_DEVICE_CLASS(PCI_CLASS_DISPLAY_VGA << 8, 0xffff00) },
	{ PCI_DEVICE_CLASS(PCI_CLASS_DISPLAY_OTHER << 8, 0xffff00) },
	{ PCI_DEVICE_CLASS(PCI_CLASS_ACCELERATOR_PROCESSING << 8, 0xffff00) },
	{ 0, }
};
MODULE_DEVICE_TABLE(pci, ramshared_pci_tbl);

static struct pci_driver ramshared_pci_driver = {
	.name		= RAMSHARED_DRIVER_NAME,
	.id_table	= ramshared_pci_tbl,
	.probe		= ramshared_pci_probe,
	.remove		= ramshared_pci_remove,
	.err_handler	= &ramshared_pci_err_handler,
};

module_pci_driver(ramshared_pci_driver);
