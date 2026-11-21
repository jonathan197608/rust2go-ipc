package main

import (
	"crypto/md5"
	"encoding/hex"
	"os"
)

type Golang struct{}

func init() { GolangCallImpl = Golang{} }

func (Golang) md5(req *HashRequest) HashResponse {
	sum := md5.Sum([]byte(req.message))
	return HashResponse{hash: hex.EncodeToString(sum[:])}
}

func (Golang) echoList(req *ListRequest) ListResponse {
	return ListResponse{
		raw:   req.raw,
		names: req.names,
		items: req.items,
		count: req.count,
		flag:  req.flag,
	}
}

func (Golang) Shutdown(req *EmptyRequest) { os.Exit(0) }
