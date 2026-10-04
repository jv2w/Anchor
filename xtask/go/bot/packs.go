package main

import (
	"bytes"
	"crypto/sha256"
	"fmt"

	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// packDownload downloads the resource packs a server offers the way gophertunnel's client does: it asks
// for every pack at once, then for each chunk of one pack after the previous chunk arrived, and checks
// every pack's size and SHA-256 against its ResourcePackDataInfo.
type packDownload struct {
	offered int
	queue   []*downloadingPack
	current *downloadingPack
	done    int
}

// downloadingPack is one pack being downloaded.
type downloadingPack struct {
	info packet.ResourcePackDataInfo
	data bytes.Buffer
	next uint32
}

// packsInfo answers ResourcePacksInfo: with packs offered it asks for all of them, else it has them all.
func (s *session) packsInfo(pk *packet.ResourcePacksInfo) error {
	if len(pk.TexturePacks) == 0 {
		return s.send(&packet.ResourcePackClientResponse{Response: packet.PackResponseAllPacksDownloaded})
	}
	s.packs = &packDownload{offered: len(pk.TexturePacks)}
	s.res.packsOffered = len(pk.TexturePacks)
	ids := make([]string, 0, len(pk.TexturePacks))
	for _, p := range pk.TexturePacks {
		ids = append(ids, p.UUID.String()+"_"+p.Version)
	}
	return s.send(&packet.ResourcePackClientResponse{Response: packet.PackResponseSendPacks, PacksToDownload: ids})
}

// packDataInfo queues a pack to download and starts it when no other one is in progress.
func (s *session) packDataInfo(pk *packet.ResourcePackDataInfo) error {
	if s.packs == nil {
		return fmt.Errorf("data info for %v without an offer", pk.UUID)
	}
	if pk.DataChunkSize == 0 {
		return fmt.Errorf("data info for %v has chunk size 0", pk.UUID)
	}
	s.packs.queue = append(s.packs.queue, &downloadingPack{info: *pk})
	if s.packs.current == nil {
		return s.nextPack()
	}
	return nil
}

// nextPack requests the first chunk of the next queued pack.
func (s *session) nextPack() error {
	d := s.packs
	if len(d.queue) == 0 {
		d.current = nil
		return nil
	}
	d.current, d.queue = d.queue[0], d.queue[1:]
	return s.send(&packet.ResourcePackChunkRequest{UUID: d.current.info.UUID, ChunkIndex: 0})
}

// packChunk adds a chunk to the pack in progress, checks a finished pack and asks for what follows.
func (s *session) packChunk(pk *packet.ResourcePackChunkData) error {
	if s.packs == nil || s.packs.current == nil {
		return fmt.Errorf("chunk of %v without a download", pk.UUID)
	}
	cur := s.packs.current
	if pk.UUID != cur.info.UUID || pk.ChunkIndex != cur.next || pk.DataOffset != uint64(cur.data.Len()) {
		return fmt.Errorf("chunk %v of %v at %v, want chunk %v of %v at %v",
			pk.ChunkIndex, pk.UUID, pk.DataOffset, cur.next, cur.info.UUID, cur.data.Len())
	}
	cur.data.Write(pk.Data)
	cur.next++
	if uint64(cur.data.Len()) < cur.info.Size {
		if len(pk.Data) != int(cur.info.DataChunkSize) {
			return fmt.Errorf("chunk %v of %v holds %v bytes, want %v", pk.ChunkIndex, pk.UUID, len(pk.Data), cur.info.DataChunkSize)
		}
		return s.send(&packet.ResourcePackChunkRequest{UUID: cur.info.UUID, ChunkIndex: int32(cur.next)})
	}
	if uint64(cur.data.Len()) != cur.info.Size || cur.next != cur.info.ChunkCount {
		return fmt.Errorf("pack %v: %v bytes in %v chunks, want %v in %v",
			pk.UUID, cur.data.Len(), cur.next, cur.info.Size, cur.info.ChunkCount)
	}
	sum := sha256.Sum256(cur.data.Bytes())
	if !bytes.Equal(sum[:], cur.info.Hash) {
		return fmt.Errorf("pack %v: sha-256 differs from the data info", pk.UUID)
	}
	s.packs.done++
	s.res.packsDownloaded = s.packs.done
	if err := s.nextPack(); err != nil || s.packs.current != nil {
		return err
	}
	if s.packs.done != s.packs.offered {
		return fmt.Errorf("downloaded %v packs of %v offered", s.packs.done, s.packs.offered)
	}
	return s.send(&packet.ResourcePackClientResponse{Response: packet.PackResponseAllPacksDownloaded})
}
